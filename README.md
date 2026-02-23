# auto-makemkv

Automated disc ripping daemon that monitors optical drives, rips inserted discs using [MakeMKV](https://www.makemkv.com/), and ejects them when done. Supports multiple drives simultaneously.

## Features

- **Automatic drive discovery** — detects all connected optical drives at startup
- **Disc monitoring** — polls drives and starts ripping when a new disc is detected
- **Multi-drive support** — rips from multiple drives concurrently with independent polling loops
- **Progress reporting** — interactive progress bars in terminals, structured log output otherwise
- **File size tracking** — monitors output file sizes during rips to show real throughput
- **Duplicate detection** — tracks disc signatures to avoid re-ripping the same disc
- **Auto-eject** — ejects the disc after a successful rip (macOS and Linux)
- **Live reconfiguration** — send `SIGUSR1` to re-discover drives without restarting
- **Graceful shutdown** — `Ctrl+C` signals all tasks to finish cleanly

## Requirements

- [MakeMKV](https://www.makemkv.com/) installed with `makemkvcon` on your `PATH`
- One or more optical disc drives

## Installation

### From source

```bash
cargo build --release
```

The binary will be at `target/release/auto-makemkv`.

### Docker

A Docker image is published to GitHub Container Registry on every push to `main`. The image is based on [automaticrippingmachine/arm-dependencies](https://github.com/automatic-ripping-machine/automatic-ripping-machine) which includes MakeMKV and other required tools.

```bash
docker pull ghcr.io/jordansekky/auto-makemkv:main
```

```bash
docker run --rm \
  --device /dev/sr0 \
  --device /dev/sg0 \
  -v /path/to/output:/output \
  -e AUTO_MAKEMKV_OUTPUT_DIR=/output \
  ghcr.io/jordansekky/auto-makemkv:main
```

## Usage

```
auto-makemkv [OPTIONS]
```

### Options

| Flag | Env Variable | Default | Description |
|---|---|---|---|
| `-o`, `--output-dir` | `AUTO_MAKEMKV_OUTPUT_DIR` | `./output` | Directory to store ripped MKV files |
| `--min-length` | `AUTO_MAKEMKV_MIN_LENGTH` | `120` | Minimum title length in seconds |
| `--poll-interval` | `AUTO_MAKEMKV_POLL_INTERVAL` | `5` | Seconds between drive polls |
| `-i`, `--interactive` | — | `true` if TTY | Show progress bars instead of log output |

All flags can also be set via environment variables.

### Examples

Rip all discs to a NAS mount, only keeping titles longer than 10 minutes:

```bash
auto-makemkv --output-dir /mnt/nas/rips --min-length 600
```

Run non-interactively with environment variables (useful in containers):

```bash
export AUTO_MAKEMKV_OUTPUT_DIR=/data/rips
export AUTO_MAKEMKV_MIN_LENGTH=300
export AUTO_MAKEMKV_POLL_INTERVAL=10
auto-makemkv --interactive false
```

### Output structure

Ripped files are organized as `<output_dir>/NN_<disc_name>/`, where `NN` is an auto-incrementing number to handle repeated rips of the same disc:

```
output/
├── 01_MY_MOVIE/
│   ├── title00.mkv
│   └── title01.mkv
└── 02_MY_MOVIE/
    └── title00.mkv
```

### Signals

- **SIGUSR1** — triggers drive re-discovery. Waits for any active rips to finish before reconfiguring. Useful when hot-swapping drives.
- **SIGINT / Ctrl+C** — initiates graceful shutdown, letting in-progress operations complete.

## Logging

Logging is controlled via the `RUST_LOG` environment variable (defaults to `info`):

```bash
RUST_LOG=debug auto-makemkv
```
