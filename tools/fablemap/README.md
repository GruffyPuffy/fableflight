# fablemap

`fablemap` is a standalone map-download utility for Fable Flight. It does not
change or add support to the game runtime.

The first supported package format is an inspectable directory containing raw
provider files and a versioned manifest.

## Build

```sh
cd /home/stefan/projects/fableflight
cargo run --manifest-path tools/fablemap/Cargo.toml -- --help
```

## Create a map

Around an airport:

```sh
cargo run --manifest-path tools/fablemap/Cargo.toml -- create \
  --airport ESGJ \
  --radius-km 40 \
  --output maps/esgj
```

Around coordinates:

```sh
cargo run --manifest-path tools/fablemap/Cargo.toml -- create \
  --lat 57.663 --lon 14.069 \
  --radius-km 40 \
  --output maps/esgj
```

The default imagery zoom range is 8 through 14. Elevation and vector data are
downloaded at zoom 14, matching the game's current data zoom. Use `--max-zoom`
to request more imagery detail; larger ranges can be very large.

Coverage adds a 10 km download buffer around the requested radius. Zoom 14
elevation and vector grids also include a one-tile border on every side so
runway leveling and neighbouring tile reads do not stop at the package edge.

Downloaded content is resumable: rerunning `create` with the same output
directory skips existing non-empty files.

## Inspect and verify

```sh
cargo run --manifest-path tools/fablemap/Cargo.toml -- info maps/esgj
cargo run --manifest-path tools/fablemap/Cargo.toml -- verify maps/esgj
```

## Test a package in the game

The runtime currently supports package loading for direct airport starts:

```sh
cargo run -- --map maps/mini-caucasus --airport UGKO --runway 07 --aircraft c172 --mute
```

Map mode disables network tile fetching and reads imagery, elevation and vector
tiles directly from the package. Google imagery and photorealistic tiles are
also disabled while `--map` is active.

Rerunning `create` with the same output resumes existing files. The package
manifest is marked incomplete after creation starts and is marked complete only
after every requested download succeeds.

To enlarge an existing package without changing its centre:

```sh
cargo run --manifest-path tools/fablemap/Cargo.toml -- extend maps/esgj --radius-km 100
```

## Package layout

```text
MAP/
  manifest.json
  imagery/{z}/{x}/{y}.jpg
  elevation/{z}/{x}/{y}.png
  vector/{z}/{x}/{y}.pbf
  patches/
  overlays/
```

`patches/` and `overlays/` are reserved for future local geometry and DCS-style
content. The utility intentionally does not read or write game code.
