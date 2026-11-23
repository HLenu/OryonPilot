#!/usr/bin/env python3
"""Build and package the NeuroSched KernelSU module.

Steps
-----
1. cross-compile the daemon for aarch64 (musl static, no runtime dependency)
2. generate the default config from the daemon itself (no duplicated defaults)
3. stage the module tree and write `module.prop` with the real version
4. zip it with unix permissions preserved, verify the archive, write SHA256SUMS

Usage
-----
    python scripts/build.py                # build + package + verify
    python scripts/build.py --check        # also run fmt/clippy/test first
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import shutil
import struct
import subprocess
import sys
import zipfile
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
DAEMON = ROOT / "daemon"
MODULE = ROOT / "module"
DIST = ROOT / "dist"
TARGET = "aarch64-unknown-linux-musl"
ANDROID_TARGET = "aarch64-linux-android"
BIN_NAME = "neuroschedd"

EXECUTABLES = {
    "customize.sh",
    "service.sh",
    "boot-completed.sh",
    "uninstall.sh",
    "action.sh",
    f"bin/{BIN_NAME}",
}


def run(cmd: list[str], cwd: Path | None = None, env: dict | None = None) -> str:
    printable = " ".join(cmd)
    print(f"  $ {printable}")
    full_env = os.environ.copy()
    if env:
        full_env.update(env)
    proc = subprocess.run(cmd, cwd=str(cwd or ROOT), env=full_env,
                          capture_output=True, text=True)
    if proc.returncode != 0:
        sys.stderr.write(proc.stdout)
        sys.stderr.write(proc.stderr)
        raise SystemExit(f"command failed ({proc.returncode}): {printable}")
    return proc.stdout


PAGE_ALIGN = "16384"


def base_env() -> dict:
    """PATH with the rustup shims, nothing else. Used for host-side commands."""
    env = {}
    cargo_bin = Path.home() / ".cargo" / "bin"
    if cargo_bin.is_dir():
        env["PATH"] = str(cargo_bin) + os.pathsep + os.environ.get("PATH", "")
    return env


def cross_env() -> dict:
    """RUSTFLAGS for the aarch64 musl build.

    The binary must be usable on both 4 KiB and 16 KiB page kernels (Android 15+ ships
    devices with either), so PT_LOAD segments are aligned to 16 KiB even though the
    kernel page size is not known at build time. `rust-lld` is the linker, so the
    arguments go through `-C link-arg` rather than `-Wl,`.
    """
    flags = [
        "-C linker=rust-lld",
        "-C link-arg=-z",
        "-C link-arg=max-page-size=16384",
        "-C link-arg=-z",
        "-C link-arg=common-page-size=16384",
    ]
    env = base_env()
    env["RUSTFLAGS"] = " ".join(flags)
    return env


def version() -> tuple[str, int]:
    lib = (DAEMON / "src" / "lib.rs").read_text(encoding="utf-8")
    ver = re.search(r'pub const VERSION: &str = "([^"]+)"', lib)
    code = re.search(r"pub const VERSION_CODE: u32 = (\d+)", lib)
    if not ver or not code:
        raise SystemExit("cannot read VERSION / VERSION_CODE from daemon/src/lib.rs")
    return ver.group(1), int(code.group(1))


def check_toolchain(ndk: bool = False) -> None:
    print("[1/6] toolchain")
    out = run(["cargo", "--version"], env=base_env())
    print(f"  {out.strip()}")
    targets = run(["rustup", "target", "list", "--installed"], env=base_env())
    target = ANDROID_TARGET if ndk else TARGET
    if target not in targets.splitlines():
        raise SystemExit(
            f"target {target} missing. Run: rustup target add {target}"
        )


def check_quality() -> None:
    print("[2/6] quality gates (fmt / clippy / test)")
    env = base_env()
    run(["cargo", "fmt", "--all", "--", "--check"], cwd=DAEMON, env=env)
    run(["cargo", "clippy", "--locked", "--offline", "--all-targets", "--", "-D", "warnings"], cwd=DAEMON, env=env)
    run(["cargo", "test", "--locked", "--offline", "--quiet"], cwd=DAEMON, env=env)


def cargo_target_dir() -> Path:
    metadata = json.loads(run(
        ["cargo", "metadata", "--locked", "--offline", "--no-deps", "--format-version", "1"],
        cwd=DAEMON, env=base_env(),
    ))
    return Path(metadata["target_directory"])


def build_binary(skip: bool, ndk: bool = False) -> Path:
    print("[3/6] cross-compile")
    target = ANDROID_TARGET if ndk else TARGET
    if skip:
        raise SystemExit("--skip-build is disabled. Packaging requires a current locked build.")
    binary = cargo_target_dir() / target / "release" / BIN_NAME

    if ndk:
        # Alternative toolchain: bionic via the NDK. Kept because it is the officially
        # supported Android target, but it has NOT been exercised in this environment
        # (no NDK installed), so the shipped artifact stays musl.
        ndk_home = os.environ.get("ANDROID_NDK_HOME") or os.environ.get("ANDROID_NDK_ROOT")
        if not ndk_home:
            raise SystemExit(
                "--ndk requires ANDROID_NDK_HOME (or ANDROID_NDK_ROOT) pointing at an NDK r26+"
            )
        host_tag = {"nt": "windows-x86_64", "posix": "linux-x86_64"}.get(os.name, "linux-x86_64")
        linker = Path(ndk_home) / "toolchains" / "llvm" / "prebuilt" / host_tag / "bin" / "aarch64-linux-android30-clang"
        if not linker.exists():
            raise SystemExit(f"NDK linker not found: {linker}")
        env = base_env()
        env["RUSTFLAGS"] = f"-C linker={linker}"
        env["CC_aarch64_linux_android"] = str(linker)
        env["AR_aarch64_linux_android"] = str(
            Path(ndk_home) / "toolchains" / "llvm" / "prebuilt" / host_tag / "bin" / "llvm-ar"
        )
        targets = run(["rustup", "target", "list", "--installed"], env=env)
        if ANDROID_TARGET not in targets:
            raise SystemExit(f"target {ANDROID_TARGET} missing. Run: rustup target add {ANDROID_TARGET}")
        run(["cargo", "build", "--locked", "--offline", "--release", "--target", ANDROID_TARGET], cwd=DAEMON, env=env)
    else:
        run(["cargo", "build", "--locked", "--offline", "--release", "--target", TARGET], cwd=DAEMON, env=cross_env())

    if not binary.exists():
        raise SystemExit(f"missing binary: {binary}")
    verify_elf(binary, require_static=not ndk)
    return binary


def verify_elf(binary: Path, require_static: bool = True) -> None:
    data = binary.read_bytes()
    if data[:4] != b"\x7fELF":
        raise SystemExit("binary is not an ELF file")
    if len(data) < 64:
        raise SystemExit("truncated ELF64 header")
    if data[4] != 2:
        raise SystemExit("binary is not 64-bit")
    if data[5] != 1:
        raise SystemExit("binary is not little-endian")
    if data[6] != 1:
        raise SystemExit("unsupported ELF identification version")
    (
        elf_type, machine, elf_version, _, phoff, _, _, ehsize,
        phentsize, phnum, _, _, _,
    ) = struct.unpack_from("<HHIQQQIHHHHHH", data, 16)
    if machine != 183:
        raise SystemExit(f"binary is not aarch64 (e_machine={machine})")
    if elf_type not in (2, 3) or elf_version != 1 or ehsize != 64:
        raise SystemExit("invalid ELF64 executable header")
    if phnum == 0 or phnum == 0xFFFF or phentsize != 56:
        raise SystemExit("invalid or unsupported ELF64 program header table")
    if phoff < ehsize or phoff + phnum * phentsize > len(data):
        raise SystemExit("ELF64 program header table is outside the file")

    # 16 KiB page compatibility: every PT_LOAD must be aligned to at least 16 KiB,
    # otherwise a device with 16 KiB pages refuses to map the image.
    aligns = []
    mapped_bytes = 0
    has_interp = False
    has_needed = False
    has_dynamic_table = False
    address_limit = (1 << 64) - 1
    for i in range(phnum):
        (
            segment_type, _, offset, vaddr, _, filesz, memsz, align,
        ) = struct.unpack_from("<IIQQQQQQ", data, phoff + i * phentsize)
        if segment_type == 0:  # PT_NULL has no defined segment fields.
            continue
        if offset > len(data) or filesz > len(data) - offset:
            raise SystemExit(f"program segment {i} is outside the file")
        if vaddr + memsz > address_limit:
            raise SystemExit(f"program segment {i} overflows its virtual address range")
        if align > 1 and align & (align - 1):
            raise SystemExit(f"program segment {i} has non-power-of-two alignment")
        if segment_type == 1:  # PT_LOAD
            if filesz > memsz:
                raise SystemExit(f"PT_LOAD segment {i} has filesz greater than memsz")
            if align < int(PAGE_ALIGN):
                raise SystemExit(f"PT_LOAD alignment {align} < {PAGE_ALIGN} (16 KiB page unsafe)")
            if offset % align != vaddr % align:
                raise SystemExit(f"PT_LOAD segment {i} offset/vaddr alignment mismatch")
            aligns.append(align)
            mapped_bytes += memsz
        elif segment_type == 3:  # PT_INTERP
            if has_interp:
                raise SystemExit("multiple PT_INTERP segments")
            interpreter = data[offset:offset + filesz]
            if len(interpreter) < 2 or interpreter[-1] != 0 or 0 in interpreter[:-1]:
                raise SystemExit("invalid PT_INTERP interpreter path")
            has_interp = True
        elif segment_type == 2:  # PT_DYNAMIC, an array of Elf64_Dyn entries.
            if has_dynamic_table or filesz == 0 or filesz % 16:
                raise SystemExit("invalid PT_DYNAMIC table")
            has_dynamic_table = True
            terminated = False
            for entry_offset in range(offset, offset + filesz, 16):
                tag, _ = struct.unpack_from("<qQ", data, entry_offset)
                if tag == 0:  # DT_NULL terminates the table.
                    terminated = True
                    break
                if tag == 1:  # DT_NEEDED is an actual runtime dependency.
                    has_needed = True
            if not terminated:
                raise SystemExit("PT_DYNAMIC table has no DT_NULL terminator")
    if not aligns or mapped_bytes == 0:
        raise SystemExit("no nonempty PT_LOAD segment found")
    dynamic = has_interp or has_needed
    if dynamic and require_static:
        raise SystemExit("binary is dynamically linked (PT_INTERP or DT_NEEDED); a static build is required")
    if dynamic:
        print("  note: dynamic build — expected for the NDK target")
    linkage = "dynamic" if dynamic else "static"
    print(
        f"  ELF aarch64 {linkage}, {len(data) // 1024} KiB, "
        f"PT_LOAD align {min(aligns)}, e_type={elf_type}"
    )


def generate_default_config(binary_host: Path) -> dict:
    print("[4/6] default config")
    cfg = json.loads(run([str(binary_host), "--print-default-config"]))
    verify_safe_defaults(cfg)
    return cfg


def verify_safe_defaults(cfg: dict) -> None:
    if not isinstance(cfg, dict) or cfg.get("mode") != "observe_only":
        raise SystemExit("default config must ship in observe_only mode")
    actuator = cfg.get("actuator")
    if not isinstance(actuator, dict) or actuator.get("enabled") is not False:
        raise SystemExit("default config must explicitly contain actuator.enabled=false")
    sys_source = (DAEMON / "src" / "sys.rs").read_text(encoding="utf-8")
    if not re.search(r"^pub const ACTUATOR_ENABLED: bool = false;\s*$", sys_source, re.M):
        raise SystemExit("production ACTUATOR_ENABLED hard gate must be false")


def stage(binary: Path, cfg: dict, ver: str, code: int) -> Path:
    print("[5/6] staging module")
    verify_safe_defaults(cfg)
    stage_dir = DIST / "stage"
    if stage_dir.exists():
        shutil.rmtree(stage_dir)
    stage_dir.mkdir(parents=True)

    for name in ["customize.sh", "service.sh", "boot-completed.sh", "uninstall.sh", "action.sh"]:
        shutil.copy2(MODULE / name, stage_dir / name)

    shutil.copytree(MODULE / "webroot", stage_dir / "webroot")
    shutil.copy2(ROOT / "LICENSE", stage_dir / "LICENSE")
    notices = stage_dir / "licenses" / "tiny_http"
    notices.mkdir(parents=True)
    for name in ["LICENSE-MIT", "LICENSE-APACHE", "PATCHES.md"]:
        shutil.copy2(DAEMON / "vendor" / "tiny_http" / name, notices / name)
    (stage_dir / "bin").mkdir()
    shutil.copy2(binary, stage_dir / "bin" / BIN_NAME)

    (stage_dir / "config.default.json").write_text(
        json.dumps(cfg, indent=2, ensure_ascii=False) + "\n", encoding="utf-8"
    )

    prop = (MODULE / "module.prop").read_text(encoding="utf-8")
    prop = re.sub(r"^version=.*$", f"version=v{ver}", prop, flags=re.M)
    prop = re.sub(r"^versionCode=.*$", f"versionCode={code}", prop, flags=re.M)
    (stage_dir / "module.prop").write_text(prop, encoding="utf-8")

    # Unix modes inside the archive (Magisk/KSU apply their own set_perm too).
    for path in stage_dir.rglob("*"):
        rel = path.relative_to(stage_dir).as_posix()
        if path.is_dir():
            path.chmod(0o755)
        elif rel in EXECUTABLES:
            path.chmod(0o755)
        else:
            path.chmod(0o644)
    print(f"  staged {sum(1 for _ in stage_dir.rglob('*'))} entries")
    return stage_dir


def package(stage_dir: Path, ver: str) -> Path:
    print("[6/6] packaging")
    DIST.mkdir(exist_ok=True)
    zip_path = DIST / f"neurosched-v{ver}-arm64.zip"
    if zip_path.exists():
        zip_path.unlink()

    with zipfile.ZipFile(zip_path, "w", zipfile.ZIP_DEFLATED, compresslevel=9) as zf:
        for path in sorted(stage_dir.rglob("*")):
            rel = path.relative_to(stage_dir).as_posix()
            if path.is_dir():
                info = zipfile.ZipInfo(rel + "/")
                info.external_attr = (0o40755 << 16) | 0x10
                zf.writestr(info, b"")
                continue
            info = zipfile.ZipInfo(rel)
            info.compress_type = zipfile.ZIP_DEFLATED
            mode = 0o100755 if rel in EXECUTABLES else 0o100644
            info.external_attr = mode << 16
            zf.writestr(info, path.read_bytes())

    verify_zip(zip_path, ver)
    digest = hashlib.sha256(zip_path.read_bytes()).hexdigest()
    sums = DIST / "SHA256SUMS"
    sums.write_text(f"{digest}  {zip_path.name}\n", encoding="utf-8")
    print(f"  {zip_path.name}  {zip_path.stat().st_size // 1024} KiB")
    print(f"  sha256 {digest}")
    return zip_path


def verify_zip(zip_path: Path, ver: str) -> None:
    required = {
        "module.prop",
        "customize.sh",
        "service.sh",
        "boot-completed.sh",
        "uninstall.sh",
        "action.sh",
        "bin/neuroschedd",
        "webroot/index.html",
        "config.default.json",
        "LICENSE",
        "licenses/tiny_http/LICENSE-MIT",
        "licenses/tiny_http/LICENSE-APACHE",
        "licenses/tiny_http/PATCHES.md",
    }
    with zipfile.ZipFile(zip_path) as zf:
        names = set(zf.namelist())
        missing = required - names
        if missing:
            raise SystemExit(f"zip missing: {sorted(missing)}")
        for name in names:
            if name.startswith("/") or ".." in Path(name).parts:
                raise SystemExit(f"unsafe zip entry: {name}")
        prop = zf.read("module.prop").decode()
        if f"version=v{ver}" not in prop:
            raise SystemExit("module.prop version mismatch")
        if "id=neurosched" not in prop:
            raise SystemExit("module.prop id mismatch")
        binfo = zf.getinfo("bin/neuroschedd")
        if not (binfo.external_attr >> 16) & 0o111:
            raise SystemExit("binary is not marked executable in the archive")
        elf = zf.read("bin/neuroschedd")[:20]
        if elf[:4] != b"\x7fELF" or struct.unpack("<H", elf[18:20])[0] != 183:
            raise SystemExit("packaged binary is not aarch64 ELF")
        cfg = json.loads(zf.read("config.default.json").decode())
        verify_safe_defaults(cfg)
    print(f"  verified {len(names)} entries, observe_only and disabled actuator confirmed")


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--skip-build", action="store_true", help="unsupported, a current build is required")
    ap.add_argument(
        "--ndk",
        action="store_true",
        help="build with aarch64-linux-android via the NDK instead of musl (untested here)",
    )
    ap.add_argument("--check", action="store_true", help="run fmt/clippy/test first")
    args = ap.parse_args()

    ver, code = version()
    print(f"NeuroSched v{ver} (versionCode {code})")
    check_toolchain(args.ndk)
    if args.check:
        check_quality()
    binary = build_binary(args.skip_build, args.ndk)

    run(["cargo", "build", "--locked", "--offline"], cwd=DAEMON, env=base_env())
    host_bin = cargo_target_dir() / "debug" / ("neuroschedd.exe" if os.name == "nt" else "neuroschedd")
    cfg = generate_default_config(host_bin)

    stage_dir = stage(binary, cfg, ver, code)
    zip_path = package(stage_dir, ver)

    print()
    print(f"OK  {zip_path}")
    print(f"    {DIST / 'SHA256SUMS'}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
