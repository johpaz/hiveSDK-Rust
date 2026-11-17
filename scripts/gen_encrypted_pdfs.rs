// Generator of the AES (R4) and AES-256 (R5, R6) fixtures in crates/hive-core/tests/fixtures/pdf-encrypted/,
// written from the PDF specification, NOT from the reader. Each file was checked with poppler's pdftotext (an independent
// implementation) before being committed. The RC4 files (r2-*, r3-*) were made by Ghostscript:
//   gs -q -dNOPAUSE -dBATCH -sDEVICE=pdfwrite -dEncryptionR=<2|3> -dKeyLength=<40|128> -sOwnerPassword=owner [-sUserPassword=secreto] -o out.pdf plain.pdf
// Build: a Cargo project with aes 0.8, cbc 0.1 (feature alloc), md5 0.8 and sha2 0.10; run with the output directory as argument.
use aes::cipher::{BlockEncryptMut, KeyIvInit, block_padding::{NoPadding, Pkcs7}};
use sha2::{Digest, Sha256, Sha384, Sha512};

const PAD: [u8; 32] = [0x28,0xBF,0x4E,0x5E,0x4E,0x75,0x8A,0x41,0x64,0x00,0x4E,0x56,0xFF,0xFA,0x01,0x08,0x2E,0x2E,0x00,0xB6,0xD0,0x68,0x3E,0x80,0x2F,0x0C,0xA9,0xFE,0x64,0x53,0x69,0x7A];
fn md5(parts: &[&[u8]]) -> Vec<u8> { md5::compute(parts.concat()).0.to_vec() }
fn rc4(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut s: Vec<u8> = (0..=255).collect(); let mut j = 0u8;
    for i in 0..256 { j = j.wrapping_add(s[i]).wrapping_add(key[i % key.len()]); s.swap(i, j as usize); }
    let (mut i, mut j) = (0u8, 0u8);
    data.iter().map(|b| { i = i.wrapping_add(1); j = j.wrapping_add(s[i as usize]); s.swap(i as usize, j as usize); b ^ s[s[i as usize].wrapping_add(s[j as usize]) as usize] }).collect()
}
fn pad(pw: &[u8]) -> Vec<u8> { let mut p = pw.to_vec(); p.truncate(32); p.extend_from_slice(&PAD[..32 - p.len()]); p }
fn cbc128(key: &[u8], iv: &[u8], data: &[u8], padded: bool) -> Vec<u8> {
    let e = cbc::Encryptor::<aes::Aes128>::new_from_slices(key, iv).unwrap();
    if padded { e.encrypt_padded_vec_mut::<Pkcs7>(data) } else { e.encrypt_padded_vec_mut::<NoPadding>(data) }
}
fn cbc256(key: &[u8], iv: &[u8], data: &[u8], padded: bool) -> Vec<u8> {
    let e = cbc::Encryptor::<aes::Aes256>::new_from_slices(key, iv).unwrap();
    if padded { e.encrypt_padded_vec_mut::<Pkcs7>(data) } else { e.encrypt_padded_vec_mut::<NoPadding>(data) }
}
fn hash2b(pw: &[u8], salt: &[u8], udata: &[u8]) -> Vec<u8> {
    let mut k = Sha256::digest([pw, salt, udata].concat()).to_vec(); let mut i = 0usize;
    loop {
        let unit = [pw, &k, udata].concat(); let k1: Vec<u8> = unit.iter().copied().cycle().take(unit.len() * 64).collect();
        let e = cbc128(&k[..16], &k[16..32], &k1, false);
        let m = e[..16].iter().map(|b| *b as u32).sum::<u32>() % 3;
        k = match m { 0 => Sha256::digest(&e).to_vec(), 1 => Sha384::digest(&e).to_vec(), _ => Sha512::digest(&e).to_vec() };
        i += 1; if i >= 64 && (*e.last().unwrap() as usize) <= i - 32 { break; }
    }
    k.truncate(32); k
}
fn hex(b: &[u8]) -> String { b.iter().map(|x| format!("{x:02x}")).collect() }
fn seq(n: usize, seed: u8) -> Vec<u8> { (0..n).map(|i| seed.wrapping_mul(31).wrapping_add((i as u8).wrapping_mul(7)).wrapping_add(13)).collect() }

fn build(rev: u32, user_pw: &str, title: &str) -> Vec<u8> {
    let id = seq(16, 3);
    let p: i32 = -4;
    let (obj_key, aes_mode, encrypt_dict): (Box<dyn Fn(u32) -> Vec<u8>>, u8, String);
    let text = "BT /F1 24 Tf 72 700 Td (Texto secreto: hola cifrado) Tj 0 -40 Td (Segunda linea 12345) Tj ET";
    if rev <= 4 {
        let (v, length) = if rev == 4 { (4, 128) } else { (2, 128) };
        let owner = pad(b"owner");
        let mut h = md5(&[&owner]); for _ in 0..50 { h = md5(&[&h]); }
        let okey = h[..16].to_vec();
        let mut o = rc4(&okey, &pad(user_pw.as_bytes()));
        for i in 1..=19u8 { let k: Vec<u8> = okey.iter().map(|b| b ^ i).collect(); o = rc4(&k, &o); }
        let mut kh = md5(&[&pad(user_pw.as_bytes()), &o, &(p as u32).to_le_bytes(), &id]);
        for _ in 0..50 { kh = md5(&[&kh[..16]]); }
        let key = kh[..16].to_vec();
        let mut u = rc4(&key, &md5(&[&PAD, &id]));
        for i in 1..=19u8 { let k: Vec<u8> = key.iter().map(|b| b ^ i).collect(); u = rc4(&k, &u); }
        u.extend_from_slice(&[0u8; 16]);
        let cf = if rev == 4 { " /CF << /StdCF << /CFM /AESV2 /AuthEvent /DocOpen /Length 16 >> >> /StmF /StdCF /StrF /StdCF" } else { "" };
        encrypt_dict = format!("<< /Filter /Standard /V {v} /R {rev} /Length {length} /P {p} /O <{}> /U <{}>{cf} >>", hex(&o), hex(&u));
        aes_mode = (rev == 4) as u8;
        let k2 = key.clone(); let aes = rev == 4;
        obj_key = Box::new(move |n| { let mut parts: Vec<Vec<u8>> = vec![k2.clone(), n.to_le_bytes()[..3].to_vec(), vec![0, 0]]; if aes { parts.push(b"sAlT".to_vec()); } md5(&parts.iter().map(|v| v.as_slice()).collect::<Vec<_>>())[..16].to_vec() });
    } else {
        let file_key = seq(32, 9);
        let (vs, ks) = (seq(8, 21), seq(8, 22));
        let upw = user_pw.as_bytes();
        let mut u = if rev == 5 { Sha256::digest([upw, &vs].concat()).to_vec() } else { hash2b(upw, &vs, &[]) };
        u.extend_from_slice(&vs); u.extend_from_slice(&ks);
        let ik = if rev == 5 { Sha256::digest([upw, &ks].concat()).to_vec() } else { hash2b(upw, &ks, &[]) };
        let ue = cbc256(&ik, &[0; 16], &file_key, false);
        let (ovs, oks) = (seq(8, 31), seq(8, 32));
        let mut o = if rev == 5 { Sha256::digest([&b"owner"[..], &ovs, &u].concat()).to_vec() } else { hash2b(b"owner", &ovs, &u) };
        o.extend_from_slice(&ovs); o.extend_from_slice(&oks);
        let oik = if rev == 5 { Sha256::digest([&b"owner"[..], &oks, &u].concat()).to_vec() } else { hash2b(b"owner", &oks, &u) };
        let oe = cbc256(&oik, &[0; 16], &file_key, false);
        encrypt_dict = format!("<< /Filter /Standard /V 5 /R {rev} /Length 256 /P {p} /O <{}> /U <{}> /OE <{}> /UE <{}> /Perms <{}> /CF << /StdCF << /CFM /AESV3 /AuthEvent /DocOpen /Length 32 >> >> /StmF /StdCF /StrF /StdCF >>", hex(&o), hex(&u), hex(&oe), hex(&ue), hex(&seq(16, 40)));
        aes_mode = 2;
        obj_key = Box::new(move |_| file_key.clone());
    }
    let enc = |n: u32, plain: &[u8]| -> Vec<u8> {
        let key = obj_key(n);
        match aes_mode {
            0 => rc4(&key, plain),
            1 => { let iv = seq(16, n as u8); [iv.clone(), cbc128(&key, &iv, plain, true)].concat() }
            _ => { let iv = seq(16, n as u8); [iv.clone(), cbc256(&key, &iv, plain, true)].concat() }
        }
    };
    let content = enc(4, text.as_bytes());
    let title_enc = enc(7, title.as_bytes());
    let mut out: Vec<u8> = b"%PDF-1.7\n%\xe2\xe3\xcf\xd3\n".to_vec();
    let mut offsets = vec![0usize; 8];
    let mut put = |out: &mut Vec<u8>, n: usize, body: Vec<u8>| { offsets[n] = out.len(); out.extend_from_slice(format!("{n} 0 obj\n").as_bytes()); out.extend_from_slice(&body); out.extend_from_slice(b"\nendobj\n"); };
    put(&mut out, 1, b"<< /Type /Catalog /Pages 2 0 R >>".to_vec());
    put(&mut out, 2, b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec());
    put(&mut out, 3, b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Contents 4 0 R /Resources << /Font << /F1 5 0 R >> >> >>".to_vec());
    let mut stream = format!("<< /Length {} >>\nstream\n", content.len()).into_bytes(); stream.extend_from_slice(&content); stream.extend_from_slice(b"\nendstream");
    put(&mut out, 4, stream);
    put(&mut out, 5, b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding >>".to_vec());
    put(&mut out, 6, encrypt_dict.into_bytes());
    put(&mut out, 7, format!("<< /Title <{}> >>", hex(&title_enc)).into_bytes());
    let xref = out.len();
    out.extend_from_slice(b"xref\n0 8\n0000000000 65535 f \n");
    for n in 1..8 { out.extend_from_slice(format!("{:010} 00000 n \n", offsets[n]).as_bytes()); }
    out.extend_from_slice(format!("trailer\n<< /Size 8 /Root 1 0 R /Encrypt 6 0 R /Info 7 0 R /ID [<{}> <{}>] >>\nstartxref\n{xref}\n%%EOF\n", hex(&id), hex(&id)).as_bytes());
    out
}
fn main() {
    let dir = std::env::args().nth(1).unwrap();
    for rev in [4u32, 5, 6] {
        for (name, pw) in [("empty", ""), ("pw", "secreto")] {
            std::fs::write(format!("{dir}/r{rev}-{name}.pdf"), build(rev, pw, "Título cifrado")).unwrap();
        }
    }
}
