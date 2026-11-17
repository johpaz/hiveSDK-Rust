//! The PDF standard security handler, for reading: RC4 (40 and 128 bit, revisions 2 and 3),
//! AES-128 (revision 4) and AES-256 (revisions 5 and 6). It opens a file with the **user
//! password**, which is the empty string for the common "permissions only" encryption.
//!
//! Not supported, and reported as such: public-key handlers, the owner password, custom
//! crypt filters other than Identity, and revision 5 files with no matching password.
//! Nothing here renders or checks permissions; it only recovers the bytes.
use super::invalid;
use crate::Result;
use aes::cipher::{
    BlockDecryptMut, BlockEncryptMut, KeyIvInit,
    block_padding::{NoPadding, Pkcs7},
};
use ring::digest;

const PAD: [u8; 32] = [
    0x28, 0xBF, 0x4E, 0x5E, 0x4E, 0x75, 0x8A, 0x41, 0x64, 0x00, 0x4E, 0x56, 0xFF, 0xFA, 0x01, 0x08,
    0x2E, 0x2E, 0x00, 0xB6, 0xD0, 0x68, 0x3E, 0x80, 0x2F, 0x0C, 0xA9, 0xFE, 0x64, 0x53, 0x69, 0x7A,
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Method {
    Identity,
    Rc4,
    AesV2,
    AesV3,
}

/// What the `/Encrypt` dictionary and the trailer say.
pub(super) struct Params {
    pub v: u32,
    pub r: u32,
    pub key_bits: u32,
    pub o: Vec<u8>,
    pub u: Vec<u8>,
    pub ue: Vec<u8>,
    pub p: i64,
    pub encrypt_metadata: bool,
    pub stream: Method,
    pub string: Method,
    /// First element of the trailer `/ID`.
    pub id0: Vec<u8>,
}

pub(super) struct Crypt {
    key: Vec<u8>,
    pub stream: Method,
    pub string: Method,
    pub encrypt_metadata: bool,
    pub description: &'static str,
}

fn rc4(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut state: Vec<u8> = (0..=255).collect();
    let mut j = 0u8;
    for i in 0..256 {
        j = j.wrapping_add(state[i]).wrapping_add(key[i % key.len()]);
        state.swap(i, usize::from(j));
    }
    let (mut i, mut j) = (0u8, 0u8);
    data.iter()
        .map(|byte| {
            i = i.wrapping_add(1);
            j = j.wrapping_add(state[usize::from(i)]);
            state.swap(usize::from(i), usize::from(j));
            let k = state[usize::from(state[usize::from(i)].wrapping_add(state[usize::from(j)]))];
            byte ^ k
        })
        .collect()
}

fn md5(parts: &[&[u8]]) -> [u8; 16] {
    let mut all = Vec::new();
    for part in parts {
        all.extend_from_slice(part);
    }
    md5::compute(&all).0
}

fn sha(algorithm: &'static digest::Algorithm, data: &[u8]) -> Vec<u8> {
    digest::digest(algorithm, data).as_ref().to_vec()
}

fn aes_cbc_decrypt(key: &[u8], iv: &[u8], data: &[u8], padded: bool) -> Result<Vec<u8>> {
    let bad = || invalid("corrupt encrypted data");
    macro_rules! run {
        ($cipher:ty) => {{
            let decryptor =
                cbc::Decryptor::<$cipher>::new_from_slices(key, iv).map_err(|_| bad())?;
            if padded {
                decryptor
                    .decrypt_padded_vec_mut::<Pkcs7>(data)
                    .map_err(|_| bad())
            } else {
                decryptor
                    .decrypt_padded_vec_mut::<NoPadding>(data)
                    .map_err(|_| bad())
            }
        }};
    }
    match key.len() {
        16 => run!(aes::Aes128),
        32 => run!(aes::Aes256),
        _ => Err(bad()),
    }
}

fn aes128_cbc_encrypt_nopad(key: &[u8], iv: &[u8], data: &[u8]) -> Vec<u8> {
    cbc::Encryptor::<aes::Aes128>::new_from_slices(key, iv)
        .expect("16-byte key and IV")
        .encrypt_padded_vec_mut::<NoPadding>(data)
}

/// Revision 6 hash (ISO 32000-2, algorithm 2.B).
fn hash_r6(password: &[u8], salt: &[u8], user_data: &[u8]) -> Vec<u8> {
    let mut input = Vec::new();
    for part in [password, salt, user_data] {
        input.extend_from_slice(part);
    }
    let mut k = sha(&digest::SHA256, &input);
    let mut round = 0usize;
    loop {
        let unit: Vec<u8> = [password, &k, user_data].concat();
        let repeated: Vec<u8> = unit.iter().copied().cycle().take(unit.len() * 64).collect();
        let e = aes128_cbc_encrypt_nopad(&k[..16], &k[16..32], &repeated);
        let remainder = e[..16].iter().map(|b| u32::from(*b)).sum::<u32>() % 3;
        k = sha(
            match remainder {
                0 => &digest::SHA256,
                1 => &digest::SHA384,
                _ => &digest::SHA512,
            },
            &e,
        );
        round += 1;
        if round >= 64 && usize::from(e[e.len() - 1]) <= round - 32 {
            break;
        }
    }
    k.truncate(32);
    k
}

fn latin1(password: &str) -> Result<Vec<u8>> {
    password
        .chars()
        .map(|c| {
            u8::try_from(u32::from(c))
                .map_err(|_| invalid("this PDF's password uses only Latin-1 characters"))
        })
        .collect()
}

impl Crypt {
    /// Check `password` (the user password) and derive the file key.
    pub(super) fn open(params: &Params, password: &str) -> Result<Self> {
        let wrong = || invalid("incorrect password for this encrypted PDF");
        if params.stream == Method::Identity && params.string == Method::Identity {
            return Err(invalid("this PDF declares encryption but encrypts nothing"));
        }
        let (key, description) = match params.r {
            2..=4 => {
                if params.v == 0 || params.v > 4 {
                    return Err(invalid("unsupported PDF encryption version"));
                }
                let length = if params.r == 2 {
                    5
                } else {
                    (params.key_bits / 8) as usize
                };
                if !(5..=16).contains(&length) || params.o.len() < 32 || params.u.len() < 16 {
                    return Err(invalid("malformed PDF encryption dictionary"));
                }
                let mut padded = latin1(password)?;
                padded.truncate(32);
                padded.extend_from_slice(&PAD[..32 - padded.len()]);
                let permissions = (params.p as u32).to_le_bytes();
                let mut parts: Vec<&[u8]> =
                    vec![&padded, &params.o[..32], &permissions, &params.id0];
                if params.r >= 4 && !params.encrypt_metadata {
                    parts.push(&[0xff; 4]);
                }
                let mut hash = md5(&parts).to_vec();
                if params.r >= 3 {
                    for _ in 0..50 {
                        hash = md5(&[&hash[..length]]).to_vec();
                    }
                }
                let key = hash[..length].to_vec();
                let matches = if params.r == 2 {
                    params.u.len() >= 32 && rc4(&key, &PAD) == params.u[..32]
                } else {
                    let mut value = rc4(&key, &md5(&[&PAD, &params.id0]));
                    for round in 1..=19u8 {
                        let round_key: Vec<u8> = key.iter().map(|b| b ^ round).collect();
                        value = rc4(&round_key, &value);
                    }
                    value[..16] == params.u[..16]
                };
                if !matches {
                    return Err(wrong());
                }
                let description = match (params.stream, length) {
                    (Method::AesV2, _) => "AES-128",
                    (_, 5) => "RC4-40",
                    _ => "RC4-128",
                };
                (key, description)
            }
            5 | 6 => {
                if params.u.len() < 48 || params.ue.len() < 32 {
                    return Err(invalid("malformed PDF encryption dictionary"));
                }
                let mut secret = password.as_bytes().to_vec();
                secret.truncate(127);
                let hash = |salt: &[u8]| {
                    if params.r == 5 {
                        sha(&digest::SHA256, &[secret.as_slice(), salt].concat())
                    } else {
                        hash_r6(&secret, salt, &[])
                    }
                };
                if hash(&params.u[32..40]) != params.u[..32] {
                    return Err(wrong());
                }
                let intermediate = hash(&params.u[40..48]);
                let key = aes_cbc_decrypt(&intermediate, &[0; 16], &params.ue[..32], false)?;
                (key, "AES-256")
            }
            _ => return Err(invalid("unsupported PDF encryption revision")),
        };
        Ok(Self {
            key,
            stream: params.stream,
            string: params.string,
            encrypt_metadata: params.encrypt_metadata,
            description,
        })
    }

    /// Decrypt the bytes of a string or stream of indirect object `number generation`.
    pub(super) fn decrypt(
        &self,
        method: Method,
        number: u32,
        generation: u16,
        data: &[u8],
    ) -> Result<Vec<u8>> {
        let object_key = |salt: bool| -> Vec<u8> {
            let (n, g) = (number.to_le_bytes(), generation.to_le_bytes());
            let mut parts: Vec<&[u8]> = vec![&self.key, &n[..3], &g];
            if salt {
                parts.push(b"sAlT");
            }
            let hash = md5(&parts);
            hash[..(self.key.len() + 5).min(16)].to_vec()
        };
        match method {
            Method::Identity => Ok(data.to_vec()),
            Method::Rc4 => Ok(rc4(&object_key(false), data)),
            Method::AesV2 | Method::AesV3 => {
                let key = if method == Method::AesV2 {
                    object_key(true)
                } else {
                    self.key.clone()
                };
                if data.len() < 16 || !(data.len() - 16).is_multiple_of(16) {
                    return Err(invalid("corrupt encrypted data"));
                }
                if data.len() == 16 {
                    return Ok(Vec::new());
                }
                aes_cbc_decrypt(&key, &data[..16], &data[16..], true)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rc4_matches_the_published_test_vectors() {
        // RFC 6229 / classic vectors.
        assert_eq!(
            rc4(b"Key", b"Plaintext"),
            [0xBB, 0xF3, 0x16, 0xE8, 0xD9, 0x40, 0xAF, 0x0A, 0xD3]
        );
        assert_eq!(rc4(b"Wiki", b"pedia"), [0x10, 0x21, 0xBF, 0x04, 0x20]);
        assert_eq!(
            rc4(b"Secret", b"Attack at dawn"),
            [
                0x45, 0xA0, 0x1F, 0x64, 0x5F, 0xC3, 0x5B, 0x38, 0x35, 0x52, 0x54, 0x4B, 0x9B, 0xF5
            ]
        );
    }

    #[test]
    fn md5_and_aes_primitives_match_known_answers() {
        assert_eq!(hex(&md5(&[b"abc"])), "900150983cd24fb0d6963f7d28e17f72");
        // FIPS-197 appendix C.1: AES-128 single block, no padding, zero IV.
        let key: Vec<u8> = (0..16).collect();
        let plain: Vec<u8> = (0..16).map(|n| (n * 0x11) as u8).collect();
        let cipher = aes128_cbc_encrypt_nopad(&key, &[0; 16], &plain);
        assert_eq!(hex(&cipher), "69c4e0d86a7b0430d8cdb78070b4c55a");
        assert_eq!(
            aes_cbc_decrypt(&key, &[0; 16], &cipher, false).unwrap(),
            plain
        );
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }
}
