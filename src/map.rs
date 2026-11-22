//! Runtime access to standalone fablemap packages.
//!
//! This module is intentionally separate from terrain generation: a package
//! supplies raw imagery, elevation and vector tiles at existing tile IDs.

use anyhow::{Context, Result, anyhow};
use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Deserialize)]
struct Manifest {
    format: String,
    format_version: u32,
    name: String,
    center: [f64; 2],
    radius_km: f64,
    zoom: Zoom,
    completed: bool,
}

#[derive(Deserialize)]
struct Zoom {
    imagery_min: u8,
    imagery_max: u8,
    data: u8,
}

pub struct MapPackage {
    pub root: PathBuf,
    pub name: String,
    pub center: [f64; 2],
    pub radius_km: f64,
    pub imagery_min_zoom: u8,
    pub imagery_max_zoom: u8,
    pub data_zoom: u8,
}

impl MapPackage {
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let root = path.as_ref();
        let manifest_path = root.join("manifest.json");
        let data = std::fs::read(&manifest_path)
            .with_context(|| format!("could not read {}", manifest_path.display()))?;
        let manifest: Manifest = serde_json::from_slice(&data)
            .context("invalid fablemap manifest")?;
        if manifest.format != "fablemap" {
            return Err(anyhow!("{} is not a fablemap package", root.display()));
        }
        if manifest.format_version != 1 {
            return Err(anyhow!("unsupported fablemap format version {}", manifest.format_version));
        }
        if !manifest.completed {
            return Err(anyhow!("map package '{}' is incomplete", manifest.name));
        }
        Ok(Self {
            root: root.to_path_buf(),
            name: manifest.name,
            center: manifest.center,
            radius_km: manifest.radius_km,
            imagery_min_zoom: manifest.zoom.imagery_min,
            imagery_max_zoom: manifest.zoom.imagery_max,
            data_zoom: manifest.zoom.data,
        })
    }

    pub fn tile(&self, kind: &str, id: crate::geo::TileId) -> Result<Vec<u8>> {
        let (dir, ext) = match kind {
            "imagery" => ("imagery", "jpg"),
            "elevation" => ("elevation", "png"),
            "vector" => ("vector", "pbf"),
            _ => return Err(anyhow!("unknown map tile kind '{kind}'")),
        };
        let path = self.root.join(format!("{dir}/{}/{}/{}.{}", id.z, id.x, id.y, ext));
        if let Ok(data) = std::fs::read(&path) {
            if !data.is_empty() {
                return Ok(data);
            }
        }
        if id.z >= 8 {
            let size = 1i64 << id.z;
            for radius in 1..=4i64 {
                for dy in -radius..=radius {
                    for dx in -radius..=radius {
                        if dx.abs() != radius && dy.abs() != radius {
                            continue;
                        }
                        let x = (id.x as i64 + dx).rem_euclid(size);
                        let y = id.y as i64 + dy;
                        if y < 0 || y >= size {
                            continue;
                        }
                        let path = self.root.join(format!("{dir}/{}/{}/{}.{}", id.z, x, y, ext));
                        if let Ok(data) = std::fs::read(&path) {
                            if !data.is_empty() {
                                return Ok(data);
                            }
                        }
                    }
                }
            }
        }
        let fallback_zoom = match kind {
            "imagery" if id.z < self.imagery_min_zoom => self.imagery_min_zoom,
            "elevation" | "vector" => self.data_zoom,
            "imagery" => id.z,
            _ => id.z,
        };
        let candidate = crate::geo::TileId::at(
            self.center[0].to_radians(),
            self.center[1].to_radians(),
            fallback_zoom,
        );
        let path = self.root.join(format!("{dir}/{}/{}/{}.{}", candidate.z, candidate.x, candidate.y, ext));
        if let Ok(data) = std::fs::read(&path) {
            if !data.is_empty() {
                return Ok(data);
            }
        }
        let path = self.root.join(format!("{dir}/{}/{}/{}.{}", id.z, id.x, id.y, ext));
        Err(anyhow!("missing map tile {}", path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mini_caucasus_manifest_loads() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("maps/mini-caucasus");
        let map = MapPackage::load(path).unwrap();
        assert_eq!(map.name, "UGKO-KUT");
    }

    #[test]
    fn mini_caucasus_fine_tiles_load_at_ugko() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("maps/mini-caucasus");
        let map = std::sync::Arc::new(MapPackage::load(path).unwrap());
        let id = crate::geo::TileId::at(42.17745f64.to_radians(), 42.48535f64.to_radians(), 14);
        let fetch = crate::fetch::Fetcher::new(std::env::temp_dir(), true);
        let src = crate::sources::Sources::with_map(
            fetch,
            String::new(),
            std::sync::Arc::new(crate::gmaps::Google::disabled()),
            Some(map),
        );
        assert!(src.imagery(id).is_ok(), "imagery tile missing at {id:?}");
        assert!(src.elev_raw(id).is_ok(), "elevation tile missing at {id:?}");
        assert!(src.vector(id).is_ok(), "vector tile missing at {id:?}");
        let tile = crate::sources::build_tile(&src, id).expect("UGKO tile should build");
        assert!(!tile.heights.is_empty());
        assert!(!tile.imagery.is_empty());
        let min = tile.heights.iter().copied().fold(f32::INFINITY, f32::min);
        let max = tile.heights.iter().copied().fold(f32::NEG_INFINITY, f32::max);
        let center = tile.heights[((crate::sources::GRID / 2) + 1) * crate::sources::GV + (crate::sources::GRID / 2) + 1];
        println!("UGKO z14 tile heights: min={min:.1}, max={max:.1}, center={center:.1}");
        let water_count = tile.water.iter().filter(|&&water| water).count();
        println!(
            "UGKO z14 tile: {} height samples, {} marked water, {} imagery mip levels",
            tile.heights.len(),
            water_count,
            tile.imagery.len()
        );
        let elevation = src.elev_raw(id).unwrap();
        let (u, v) = id.frac(42.17745f64.to_radians(), 42.48535f64.to_radians());
        println!("UGKO fine elevation: {:.1} m", elevation.sample(u, v));
        let approach = crate::geo::TileId::at(42.164338677662236f64.to_radians(), 42.383487900681004f64.to_radians(), 14);
        let approach_tile = crate::sources::build_tile(&src, approach).expect("approach tile should build");
        let approach_min = approach_tile.heights.iter().copied().fold(f32::INFINITY, f32::min);
        let approach_max = approach_tile.heights.iter().copied().fold(f32::NEG_INFINITY, f32::max);
        println!(
            "UGKO approach z14 tile: {:?}, {} height samples, {:.1}-{:.1} m",
            approach,
            approach_tile.heights.len(),
            approach_min,
            approach_max
        );
        let airport = crate::airports::Db::get().find("UGKO").unwrap();
        let geo = crate::airports::geo_for(&src, airport).unwrap();
        for runway in &geo.runways {
            for end in 0..2 {
                let (threshold, _) = runway.end(end);
                println!("UGKO runway {}{} threshold height: {:.1} m", runway.ident[end], "", geo.h(threshold));
            }
        }
        let coarse = crate::geo::TileId::at(42.17745f64.to_radians(), 42.48535f64.to_radians(), 8);
        let coarse = crate::sources::build_tile(&src, coarse).expect("coarse UGKO terrain should build");
        assert!(!coarse.heights.is_empty());
        let world_start = crate::geo::TileId::at(42.17745f64.to_radians(), 42.48535f64.to_radians(), 2);
        let world_start = crate::sources::build_tile(&src, world_start).expect("world z2 map tile should build");
        assert!(!world_start.heights.is_empty());
        assert!(!world_start.imagery.is_empty());
    }
}
