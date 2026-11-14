#!/usr/bin/env python3
"""Opt-in mounted regression for issue #228 (requires Linux /dev/fuse).

Example: python3 tests/fuse_request_starvation.py --binary target/release/argosfs

Four files issue concurrent writes and fsyncs, while a fifth thread measures
statvfs responsiveness. Unlike a two-writer test this saturates the previous
four-thread fuser event loop. No external Python packages or real disks needed.
"""

import argparse
import concurrent.futures
import os
from pathlib import Path
import subprocess
import tempfile
import threading
import time


def exercise(mount: Path, args):
    start = threading.Barrier(5)
    rounds = [threading.Barrier(4) for _ in range(args.rounds)]
    stop = threading.Event()
    samples = []
    payloads = [bytes([index + 1]) * (1024 * 1024) for index in range(4)]

    def writer(index):
        with (mount / f"writer-{index}").open("wb", buffering=0) as file:
            start.wait()
            for barrier in rounds:
                file.write(payloads[index])
                barrier.wait()
                os.fsync(file.fileno())

    def sampler():
        start.wait()
        while not stop.is_set():
            before = time.monotonic()
            os.statvfs(mount)
            samples.append((time.monotonic() - before) * 1000)
            stop.wait(0.02)

    with concurrent.futures.ThreadPoolExecutor(max_workers=5) as pool:
        writer_jobs = [pool.submit(writer, i) for i in range(4)]
        probe = pool.submit(sampler)
        try:
            for job in writer_jobs:
                job.result(timeout=180)
        finally:
            stop.set()
        probe.result(timeout=30)

    for i in range(4):
        with (mount / f"writer-{i}").open("rb", buffering=0) as file:
            assert file.seek(0, os.SEEK_END) == args.rounds * 1024 * 1024
            file.seek(0)
            assert file.read(1) == payloads[i][:1]
            file.seek(-1, os.SEEK_END)
            assert file.read(1) == payloads[i][:1]
    if not samples:
        raise AssertionError("no concurrent statvfs measurements were collected")
    print(f"rounds={args.rounds}, samples={len(samples)}, "
          f"max statvfs={max(samples):.2f} ms")
    if max(samples) > args.max_statfs_ms:
        raise AssertionError("statvfs exceeded the configured latency limit")


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", type=Path, default=Path("target/release/argosfs"))
    parser.add_argument("--rounds", type=int, default=8)
    parser.add_argument("--max-statfs-ms", type=float, default=100.0)
    args = parser.parse_args()
    binary = args.binary.resolve()
    # The normal mounted-FUSE CI fixture supplies its already mounted root.
    existing_mount = os.environ.get("ARGOSFS_COMPAT_MOUNTPOINT")
    if existing_mount:
        exercise(Path(existing_mount), args)
        return

    with tempfile.TemporaryDirectory(prefix="argosfs-228-") as directory:
        root = Path(directory)
        volume, mount = root / "vol", root / "mnt"
        volume.mkdir()
        mount.mkdir()
        subprocess.run(
            [str(binary), "mkfs", str(volume), "--k", "1", "--m", "0", "--disks", "2",
             "--compression", "none"],
            check=True, stdout=subprocess.DEVNULL, timeout=90,
        )

        with (root / "mount.log").open("w") as log:
            env = dict(os.environ)
            # Exhaust the I/O execution pool while preserving FUSE readers.
            env.update(ARGOSFS_FUSE_REQUEST_THREADS="2", ARGOSFS_FUSE_IO_WORKERS="2")
            server = subprocess.Popen(
                [str(binary), "mount", str(volume), str(mount), "--foreground"],
                env=env, stdout=log, stderr=subprocess.STDOUT,
            )
            try:
                for _ in range(100):
                    if server.poll() is not None:
                        raise RuntimeError("FUSE server exited: " + (root / "mount.log").read_text())
                    if subprocess.run(["mountpoint", "-q", str(mount)]).returncode == 0:
                        break
                    time.sleep(0.1)
                else:
                    raise TimeoutError("FUSE mount did not appear")

                exercise(mount, args)
            finally:
                subprocess.run(["fusermount3", "-uz", str(mount)], check=False)
                if server.poll() is None:
                    server.terminate()
                try:
                    server.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    server.kill()
                    server.wait()


if __name__ == "__main__":
    main()
