#!/usr/bin/env python3
"""Mounted regressions for opt-in Linux FUSE writeback caching."""
import argparse
import mmap
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import time


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", type=Path, default=Path("target/release/argosfs"))
    args = parser.parse_args()
    binary = str(args.binary.resolve())
    with tempfile.TemporaryDirectory(prefix="argosfs-kernel-writeback-") as directory:
        root = Path(directory)
        root.chmod(0o755)
        volume, mount = root / "volume", root / "mount"
        volume.mkdir()
        mount.mkdir()
        subprocess.run([binary, "mkfs", str(volume), "--k", "1", "--m", "0",
                        "--disks", "1", "--compression", "none"],
                       check=True, stdout=subprocess.DEVNULL, timeout=90)

        def start(writeback):
            command = [binary, "mount", str(volume), str(mount), "--foreground",
                       "-o", "default_permissions", "-o", "allow_other"]
            if writeback:
                command += ["-o", "kernel_writeback"]
            log_path = root / "mount.log"
            with log_path.open("w") as log:
                server = subprocess.Popen(command, stdout=log, stderr=subprocess.STDOUT)
            for _ in range(150):
                if server.poll() is not None:
                    raise RuntimeError(log_path.read_text())
                if subprocess.run(["mountpoint", "-q", str(mount)]).returncode == 0:
                    if writeback and "kernel writeback enabled" not in log_path.read_text():
                        stop(server)
                        raise RuntimeError("kernel writeback was not negotiated")
                    return server
                time.sleep(0.1)
            server.terminate()
            server.wait(timeout=20)
            raise RuntimeError(log_path.read_text())

        def stop(server):
            try:
                subprocess.run(["fusermount3", "-u", str(mount)], check=True, timeout=20)
                server.wait(timeout=20)
            finally:
                if server.poll() is None:
                    server.terminate()
                    server.wait(timeout=20)

        file = mount / "data"
        expected = bytearray(b"A" * 16384)
        server = start(True)
        try:
            file.write_bytes(expected)
            file.chmod(0o222)
        finally:
            stop(server)

        server = start(True)
        try:
            # Remount makes the partial-page fill cold. A nonowner has only
            # write permission and must not obtain a user-visible read or
            # permission to set arbitrary timestamps.
            worker = """
import ctypes, errno, os, sys
path = sys.argv[1]
fd = os.open(path, os.O_WRONLY)
try:
    assert os.pwrite(fd, b"B" * 17, 4091) == 17
    try:
        os.read(fd, 1)
    except OSError as exc:
        assert exc.errno == errno.EBADF
    else:
        raise AssertionError("write-only descriptor allowed user read")
    os.fsync(fd)
    # Exercise precisely the mtime-only, handle-bearing request shape.
    class Timespec(ctypes.Structure):
        _fields_ = [("tv_sec", ctypes.c_long), ("tv_nsec", ctypes.c_long)]
    times = (Timespec * 2)(Timespec(0, (1 << 30) - 2), Timespec(1, 0))
    libc = ctypes.CDLL(None, use_errno=True)
    assert libc.futimens(fd, times) == -1
    assert ctypes.get_errno() in (errno.EPERM, errno.EACCES)
    for target in (path, fd):
        try:
            os.utime(target, ns=(1000000000, 1000000000))
        except PermissionError:
            pass
        else:
            raise AssertionError("nonowner allowed to set explicit timestamps")
finally:
    os.close(fd)
try:
    os.open(path, os.O_RDONLY)
except PermissionError:
    pass
else:
    raise AssertionError("write-only file allowed user read")
"""
            if os.geteuid() == 0:
                subprocess.run([sys.executable, "-c", worker, str(file)], user=65534,
                               group=65534, extra_groups=[], check=True, timeout=30)
            else:
                fd = os.open(file, os.O_WRONLY)
                try:
                    assert os.pwrite(fd, b"B" * 17, 4091) == 17
                    os.fsync(fd)
                finally:
                    os.close(fd)
            expected[4091:4108] = b"B" * 17
            file.chmod(0o600)
            assert file.read_bytes() == expected
            with file.open("r+b") as handle:
                with mmap.mmap(handle.fileno(), 0) as mapped:
                    mapped[8190:8201] = b"C" * 11
                    mapped.flush()
                os.fsync(handle.fileno())
            expected[8190:8201] = b"C" * 11
            os.truncate(file, 9000)
            expected = expected[:9000]
            with file.open("ab") as handle:
                handle.write(b"append")
                handle.flush()
                os.fsync(handle.fileno())
            expected += b"append"
            fd = os.open(file, os.O_WRONLY | os.O_SYNC)
            dup = os.dup(fd)
            try:
                assert os.pwrite(fd, b"SYNC", 32) == 4
                os.close(dup)
                dup = None
                assert os.pwrite(fd, b"ok", 36) == 2
            finally:
                if dup is not None:
                    os.close(dup)
                os.close(fd)
            expected[32:38] = b"SYNCok"
            assert file.read_bytes() == expected
            fd = os.open(file, os.O_RDWR)
            try:
                os.unlink(file)
                os.pwrite(fd, b"Z", 0)
                os.fsync(fd)
                assert os.pread(fd, 1, 0) == b"Z"
            finally:
                os.close(fd)
            file.write_bytes(expected)
            with file.open("rb") as handle:
                os.fsync(handle.fileno())
        finally:
            stop(server)
        server = start(False)
        try:
            assert file.read_bytes() == expected
        finally:
            stop(server)
        print("kernel writeback mounted regression passed")


if __name__ == "__main__":
    main()
