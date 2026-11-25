# ArgosFS website

The project website is a standalone set of static HTML pages. Serve or deploy
this directory directly; no package installation, build step, external font,
CDN script, or API is required.

## Local preview

From the repository root:

```sh
python3 -m http.server 8080 --bind 127.0.0.1 --directory website
```

Open http://127.0.0.1:8080. All links and assets are relative, so the same files
also work under a subdirectory.

## Pages

- `index.html`: project overview and interactive storage lab.
- `technology.html`: architecture, control loop, and backend choices.
- `start.html`: prerequisites, clone/build, disposable host volume, mount,
  read/write, and unmount.
- `compatibility.html`: implementation scope, validation paths, and limitations.
- `research.html`: experiment questions, entry points, and artifact workflow.

`styles.css` provides shared layout and responsive styles; `app.js` adds
navigation and clipboard controls. The homepage additionally loads `lab.css`,
`lab-model.mjs` and `lab.mjs`. No runtime dependencies or build step are required.
Without JavaScript, a short static explanation remains visible.

The lab encodes and reconstructs four-byte shards using a systematic GF(256)
Reed–Solomon teaching codec, checks shard and payload integrity, and places
multiple stripes on simulated heterogeneous devices. Users can change layouts
(reset), write/read, fail/restore disks, corrupt shards, age/slow devices, add
capacity, request drain/removal, preview and apply bounded repair or rebalance
batches. Preview does not mutate the pool. Mutations invalidate stale plans.

Slots, health, latency, checksum format, placement weights and planner policies
are illustrative, not production telemetry or wire-compatible ArgosFS data.
Each disk is an independent simulated failure domain. The lab does not mount a
volume, emulate metadata, or reproduce all real autopilot gates.

Run the dependency-free model tests with:

```sh
node --test scripts/website/lab-model.test.mjs
```

## Maintaining content

Keep shared navigation and footer markup consistent across all five pages.
Implementation descriptions should follow the current code and these documents:

- `docs/architecture.md`, `docs/raw-backend.md`, and `docs/metadata-scalability.md`
- `docs/self-driving-model.md` and `docs/safety-invariants.md`
- `docs/cli.md` and `src/cli/commands.rs`
- `docs/compatibility-report.md`, `docs/limitations.md`, and `docs/testing.md`
- `docs/artifact-evaluation.md` and `scripts/experiments/`

Do not label implementation coverage as a fresh test result. In particular,
paged metadata helpers do not establish that the JSON persistence paths have
been replaced. Link performance claims to a specific reproducible run.

For visual changes, check all pages at desktop and mobile widths, including
320px. Exercise the menu using a keyboard, the lab layouts, write/read, failure, corruption, maintenance, and reset, and the
copy controls. Check JavaScript-disabled navigation and code overflow. The
compatibility table deliberately scrolls inside its region on small screens.

For quick-start changes, verify command syntax against the CLI and run the
walkthrough on a host exposing `/dev/fuse` before claiming mounted validation.
The logical members in the directory-backed lab share host storage and are not
independent physical disks.
