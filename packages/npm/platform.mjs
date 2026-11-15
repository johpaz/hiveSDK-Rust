import platforms from './platforms.json' with { type: 'json' };

export function platformKey({ platform, arch, report }) {
  const key = platform === 'linux' ? `${platform}-${arch}-${report?.header?.glibcVersionRuntime ? 'gnu' : 'musl'}`
    : platform === 'win32' ? `${platform}-${arch}-msvc` : `${platform}-${arch}`;
  if (!Object.hasOwn(platforms, key)) throw new Error(`UNSUPPORTED_PLATFORM: Hive has no prebuilt binding for ${key}`);
  return key;
}

export function nativePackage(runtime = process) {
  return `@johpaz/hive-sdk-rust-${platformKey({ platform: runtime.platform, arch: runtime.arch,
    report: runtime.platform === 'linux' ? runtime.report?.getReport() : undefined })}`;
}
