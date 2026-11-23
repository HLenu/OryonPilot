"""Build-safety regression tests. Commands and package writes are mocked.

Run with python -B scripts/test_build.py.
"""

import contextlib
import importlib.util
import io
import json
import struct
import sys
import unittest
from pathlib import Path
from unittest import mock

sys.dont_write_bytecode = True
BUILD_SCRIPT = Path(__file__).with_name("build.py")
SPEC = importlib.util.spec_from_file_location("neurosched_build", BUILD_SCRIPT)
build = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(build)

SAFE = {"mode": "observe_only", "actuator": {"enabled": False}}


def elf64(segments=None, payload=b""):
    """Create a bounded AArch64 ELF image without invoking a compiler or linker."""
    if segments is None:
        segments = [{}]
    payload_offset = 64 + 56 * len(segments)
    image_size = payload_offset + len(payload)
    ident = b"\x7fELF\x02\x01\x01" + bytes(9)
    header = ident + struct.pack(
        "<HHIQQQIHHHHHH",
        2, 183, 1, 0x400000, 64, 0, 0, 64, 56, len(segments), 0, 0, 0,
    )
    entries = []
    for segment in segments:
        segment_type = segment.get("type", 1)
        is_load = segment_type == 1
        filesz = segment.get("filesz", image_size if is_load else len(payload))
        entries.append(struct.pack(
            "<IIQQQQQQ",
            segment_type,
            segment.get("flags", 5 if is_load else 4),
            segment.get("offset", 0 if is_load else payload_offset),
            segment.get("vaddr", 0x400000 if is_load else 0),
            0,
            filesz,
            segment.get("memsz", filesz),
            segment.get("align", 16384 if is_load else 1),
        ))
    return header + b"".join(entries) + payload


class ElfVerificationTests(unittest.TestCase):
    def verify(self, data, require_static=True):
        output = io.StringIO()
        with (
            mock.patch.object(Path, "read_bytes", return_value=data),
            contextlib.redirect_stdout(output),
        ):
            build.verify_elf(Path("synthetic-elf"), require_static=require_static)
        return output.getvalue()

    def test_static_image_ignores_unreferenced_library_strings(self):
        output = self.verify(elf64(payload=b"/lib/ld-anything\x00libc.so\x00"))
        self.assertIn("ELF aarch64 static,", output)
        self.assertNotIn("dynamic", output)

    def test_interp_detected_without_known_loader_string(self):
        data = elf64(segments=[{}, {"type": 3}], payload=b"/system/custom-loader\x00")
        with self.assertRaisesRegex(SystemExit, "dynamically linked"):
            self.verify(data)
        output = self.verify(data, require_static=False)
        self.assertIn("ELF aarch64 dynamic,", output)
        self.assertIn("expected for the NDK target", output)
        self.assertNotIn("ELF aarch64 static,", output)

    def test_needed_detected_without_interp_or_library_strings(self):
        table = struct.pack("<qQqQ", 1, 7, 0, 0)
        data = elf64(segments=[{}, {"type": 2}], payload=table)
        with self.assertRaisesRegex(SystemExit, "dynamically linked"):
            self.verify(data)
        self.assertIn("ELF aarch64 dynamic,", self.verify(data, require_static=False))

    def test_dynamic_table_without_runtime_dependencies_is_static(self):
        table = struct.pack("<qQqQ", 7, 0x400000, 0, 0)
        data = bytearray(elf64(segments=[{}, {"type": 2}], payload=table))
        struct.pack_into("<H", data, 16, 3)  # ET_DYN can be a static PIE.
        self.assertIn("ELF aarch64 static,", self.verify(bytes(data)))

    def test_dynamic_entries_stop_at_dt_null(self):
        table = struct.pack("<qQqQ", 0, 0, 1, 7)
        data = elf64(segments=[{}, {"type": 2}], payload=table)
        self.assertIn("ELF aarch64 static,", self.verify(data))

    def test_truncated_headers_fail_with_controlled_error(self):
        data = elf64()
        for size in (0, 4, 5, 6, 18, 63):
            with self.subTest(size=size), self.assertRaises(SystemExit):
                self.verify(data[:size])

    def test_invalid_header_fields_are_rejected(self):
        for offset, fmt, value in (
            (4, "B", 1), (5, "B", 2), (6, "B", 0),
            (16, "H", 1), (18, "H", 62), (20, "I", 0), (52, "H", 63),
        ):
            data = bytearray(elf64())
            struct.pack_into("<" + fmt, data, offset, value)
            with self.subTest(offset=offset), self.assertRaises(SystemExit):
                self.verify(bytes(data))

    def test_invalid_program_header_tables_are_rejected(self):
        for offset, fmt, value in (
            (32, "Q", 0), (32, "Q", 65), (32, "Q", (1 << 64) - 1),
            (54, "H", 0), (54, "H", 55), (54, "H", 64),
            (56, "H", 0), (56, "H", 0xFFFF),
        ):
            data = bytearray(elf64())
            struct.pack_into("<" + fmt, data, offset, value)
            with self.subTest(offset=offset, value=value), self.assertRaises(SystemExit):
                self.verify(bytes(data))
        with self.assertRaisesRegex(SystemExit, "table is outside"):
            self.verify(elf64()[:-1])

    def test_segment_file_bounds_and_address_overflow_are_rejected(self):
        for segment in (
            {"offset": 121}, {"filesz": 121},
            {"vaddr": (1 << 64) - 16384, "memsz": 16384},
            {"vaddr": (1 << 64) - 16384, "memsz": 32768},
            {"type": 2, "offset": 4096},
            {"type": 3, "filesz": 4096},
        ):
            with self.subTest(segment=segment), self.assertRaises(SystemExit):
                self.verify(elf64(segments=[segment]))

    def test_load_filesz_cannot_exceed_memsz(self):
        with self.assertRaisesRegex(SystemExit, "filesz greater than memsz"):
            self.verify(elf64(segments=[{"memsz": 119}]))

    def test_load_alignment_and_congruence_are_required(self):
        for segment in (
            {"align": 0}, {"align": 1}, {"align": 4096}, {"align": 24576},
            {"vaddr": 0x400001}, {"align": 32768, "vaddr": 0x404000},
        ):
            with self.subTest(segment=segment), self.assertRaises(SystemExit):
                self.verify(elf64(segments=[segment]))

    def test_congruent_nonzero_load_offset_is_accepted(self):
        data = elf64(
            segments=[{"offset": 128, "vaddr": 0x400080, "filesz": 128}],
            payload=bytes(256),
        )
        self.assertIn("PT_LOAD align 16384", self.verify(data))

    def test_every_load_segment_is_checked(self):
        with self.assertRaisesRegex(SystemExit, "offset/vaddr alignment mismatch"):
            self.verify(elf64(segments=[{}, {"vaddr": 0x400001}]))

    def test_nonempty_load_segment_is_required(self):
        for segments in ([{"type": 4}], [{"filesz": 0, "memsz": 0}]):
            with self.subTest(segments=segments), self.assertRaisesRegex(
                SystemExit, "no nonempty PT_LOAD"
            ):
                self.verify(elf64(segments=segments))

    def test_invalid_interpreter_paths_are_rejected(self):
        for path in (b"", b"\x00", b"/loader", b"/loader\x00extra\x00"):
            with self.subTest(path=path), self.assertRaisesRegex(SystemExit, "PT_INTERP"):
                self.verify(elf64(segments=[{}, {"type": 3}], payload=path))
        with self.assertRaisesRegex(SystemExit, "multiple PT_INTERP"):
            self.verify(elf64(segments=[{}, {"type": 3}, {"type": 3}], payload=b"/ld\x00"))

    def test_invalid_dynamic_tables_are_rejected(self):
        for table in (b"", bytes(15), struct.pack("<qQ", 5, 7)):
            with self.subTest(table=table), self.assertRaisesRegex(SystemExit, "PT_DYNAMIC"):
                self.verify(elf64(segments=[{}, {"type": 2}], payload=table))
        with self.assertRaisesRegex(SystemExit, "PT_DYNAMIC"):
            self.verify(elf64(
                segments=[{}, {"type": 2}, {"type": 2}],
                payload=bytes(16),
            ))


class BuildSafetyTests(unittest.TestCase):
    def setUp(self):
        self.output = contextlib.redirect_stdout(io.StringIO())
        self.output.__enter__()
        self.addCleanup(self.output.__exit__, None, None, None)

    def test_generate_config_uses_print_only(self):
        with mock.patch.object(build, "run", return_value=json.dumps(SAFE)) as command:
            self.assertEqual(build.generate_default_config(Path("host-daemon")), SAFE)
            command.assert_called_once_with(["host-daemon", "--print-default-config"])

    def test_missing_disabled_actuator_rejected(self):
        for config in [
            {"mode": "observe_only"},
            {"mode": "observe_only", "actuator": {}},
            {"mode": "observe_only", "actuator": {"enabled": True}},
            {"mode": "observe_only", "actuator": {"enabled": 0}},
        ]:
            with self.subTest(config=config), self.assertRaises(SystemExit):
                build.verify_safe_defaults(config)

    def test_enabled_mode_rejected(self):
        with self.assertRaises(SystemExit):
            build.verify_safe_defaults({"mode": "enabled", "actuator": {"enabled": False}})

    def test_hardware_gate_must_be_false(self):
        source = "pub const ACTUATOR_ENABLED: bool = true;"
        with mock.patch.object(Path, "read_text", return_value=source):
            with self.assertRaises(SystemExit):
                build.verify_safe_defaults(SAFE)

    def test_quality_commands_are_locked_and_offline(self):
        with mock.patch.object(build, "run", return_value="") as command:
            build.check_quality()
        # cargo fmt does not resolve dependencies or alter the lockfile.
        for entry in command.call_args_list[1:]:
            self.assertIn("--locked", entry.args[0])
            self.assertIn("--offline", entry.args[0])

    def test_skip_build_rejected(self):
        with self.assertRaises(SystemExit):
            build.build_binary(True)

    def test_metadata_discovers_actual_target_directory(self):
        target_dir = Path("isolated-target").resolve()
        metadata = json.dumps({"target_directory": str(target_dir)})
        with mock.patch.object(build, "run", return_value=metadata) as command:
            self.assertEqual(build.cargo_target_dir(), target_dir)
            self.assertIn("--locked", command.call_args.args[0])
            self.assertIn("--offline", command.call_args.args[0])

    def test_main_builds_host_each_time(self):
        with (
            mock.patch.object(sys, "argv", ["build.py"]),
            mock.patch.object(build, "check_toolchain"),
            mock.patch.object(build, "build_binary", return_value=Path("cross-binary")),
            mock.patch.object(build, "cargo_target_dir", return_value=Path("isolated-target")),
            mock.patch.object(build, "generate_default_config", return_value=SAFE),
            mock.patch.object(build, "stage", return_value=Path("fake-stage")),
            mock.patch.object(build, "package", return_value=Path("fake-package")),
            mock.patch.object(build, "run", return_value="") as command,
        ):
            self.assertEqual(build.main(), 0)
            command.assert_called_once_with(
                ["cargo", "build", "--locked", "--offline"],
                cwd=build.DAEMON,
                env=build.base_env(),
            )


if __name__ == "__main__":
    unittest.main(verbosity=2)
