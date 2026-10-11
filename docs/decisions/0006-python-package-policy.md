# ADR 0006: Python jobs pin a curated runtime image

- Status: Accepted
- Date: 2026-10-11
- Relates: [0005](0005-python-runtime-image.md)
- Source: [Issue #5](https://github.com/Sannrox/kako/issues/5)

## Context

User Python has no network, so every import must already exist on the
runtime ([ADR 0005](0005-python-runtime-image.md)). A single current
image is simple, but replacing it can change the output of an existing
transform without any change to job bytes or inputs. Users also ask to
declare their own packages.

The run must stay offline. Building or fetching packages belongs in a
separate step, if it exists at all. The receipt already records
`runtime_image` as `python-lockfile:` plus the SHA-256 of a hashed
lockfile.

## Decision

1. **Curated, versioned images.** kako ships one Python image built from
   `runtime/python/requirements.txt`. Operators may keep previous shipped
   images in a local catalog. The identity of an image is the lockfile
   digest already used on the receipt (`python-lockfile:sha256:…`).
2. **The job pins that identity.** A Python job may set `runtime_image`.
   When it does, the runner uses that image. When it omits the field, the
   runner uses the catalog `current` pointer if present, otherwise the
   image shipped in this build. An unknown pin fails closed before the
   sandbox starts. The receipt records the image that actually ran; a pin
   and the receipt field are the same string.
3. **Who publishes.** kako releases publish images. An operator installs
   those artifacts into `KAKO_PYTHON_IMAGES` (one directory per digest,
   plus a `current` file). Transforms do not carry a lockfile for kako to
   build, and kako does not install packages for a run.
4. **Scanning and eviction.** Vulnerability review happens when an image
   is built and released. kako does not scan at run time and does not
   delete catalog entries. The operator keeps every image that still has
   a pinned job, and drops the rest.
5. **Catalog layout.** Under `KAKO_PYTHON_IMAGES`, each image is
   `<hex>/requirements.txt` whose digest must equal `<hex>`, with an
   optional `<hex>/site-packages` tree mounted read-only onto the
   interpreter. The run still has no network.

## Consequences

- Reproducing a Python run needs the pinned image (or the shipped
  lockfile, when that is the pin) in addition to job bytes and inputs.
- Upgrading the default image does not change a pinned job, as long as
  the operator still has that image in the catalog.
- An unpinned job follows `current` and can change output when the
  operator advances it.
- Per-transform environments, allow-listed extras, and a kako-owned
  builder are out of scope until a later Issue shows the curated set is
  not enough.

## Alternatives considered

- **Per-transform hashed lockfiles, built with network in a side step,
  stored content-addressed, mounted read-only.** Rejected for now: it
  adds a publisher, a cache, eviction, and a second identity besides the
  image kako already ships. The pin on a curated image gives the same
  reproduce-after-upgrade property without that machinery.
- **Hybrid: curated base plus allow-listed extras.** Rejected: two
  package sources and two identities for one run. One lockfile digest is
  enough.
- **Always use the current image; no job pin.** Rejected: that is the
  silent-change failure this Issue exists to close.
- **Network during the run.** Rejected by [ADR 0002](0002-sandboxed-transform-runner.md).
