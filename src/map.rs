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
    bounds: [f64; 4],
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
    bounds: [f64; 4],
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
            bounds: manifest.bounds,
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
        Err(anyhow!("missing map tile {}", path.display()))
    }

    pub fn contains(&self, lat: f64, lon: f64) -> bool {
        let [south, west, north, east] = self.bounds;
        let lat = lat.to_degrees();
        let lon = lon.to_degrees();
        (south..=north).contains(&lat)
            && if west <= east {
                (west..=east).contains(&lon)
            } else {
                lon >= west || lon <= east
            }
    }

    pub(crate) fn pixel_range(&self, id: crate::geo::TileId, n: usize) -> Option<[usize; 4]> {
        let [south, west, north, east] = self.bounds;
        let tile_n = (1u64 << id.z) as f64;
        if west > east {
            return None;
        }
        let x0 = crate::geo::lon_to_merc_x(west.to_radians()) * tile_n - id.x as f64;
        let x1 = crate::geo::lon_to_merc_x(east.to_radians()) * tile_n - id.x as f64;
        let y0 = crate::geo::lat_to_merc_y(north.to_radians()) * tile_n - id.y as f64;
        let y1 = crate::geo::lat_to_merc_y(south.to_radians()) * tile_n - id.y as f64;
        if x1 < 0.0 || x0 > 1.0 || y1 < 0.0 || y0 > 1.0 {
            return None;
        }
        let px0 = (x0 * n as f64).floor().max(0.0).min(n as f64) as usize;
        let px1 = (x1 * n as f64).ceil().max(0.0).min(n as f64) as usize;
        let py0 = (y0 * n as f64).floor().max(0.0).min(n as f64) as usize;
        let py1 = (y1 * n as f64).ceil().max(0.0).min(n as f64) as usize;
        (px0 < px1 && py0 < py1).then_some([px0, py0, px1, py1])
    }

    /// A low-zoom image assembled at its real world position. A local package
    /// cannot supply imagery for the rest of its parent tile.
    pub fn coarse_imagery(&self, id: crate::geo::TileId) -> Result<image::RgbaImage> {
        const N: usize = 256;
        let blank = || {
            image::RgbaImage::from_pixel(
                N as u32,
                N as u32,
                image::Rgba([63, 82, 61, 255]),
            )
        };
        let source_zoom = self.imagery_min_zoom;
        if id.z >= source_zoom {
            if self.pixel_range(id, 1).is_none() {
                return Ok(blank());
            }
            let data = self.tile("imagery", id)?;
            return Ok(image::load_from_memory(&data)?.to_rgba8());
        }

        let mut out = blank();
        let Some([x0, y0, x1, y1]) = self.pixel_range(id, N) else {
            return Ok(out);
        };
        let mut cache = std::collections::HashMap::<(u32, u32), Option<image::RgbaImage>>::new();
        for y in y0..y1 {
            for x in x0..x1 {
                let (lat, lon) = id.lat_lon((x as f64 + 0.5) / N as f64, (y as f64 + 0.5) / N as f64);
                if !self.contains(lat, lon) {
                    continue;
                }
                let sid = crate::geo::TileId::at(lat, lon, source_zoom);
                let image = cache.entry((sid.x, sid.y)).or_insert_with(|| {
                    self.tile("imagery", sid).ok().and_then(|data| image::load_from_memory(&data).ok()).map(|image| image.to_rgba8())
                });
                let Some(image) = image else {
                    continue;
                };
                let (u, v) = sid.frac(lat, lon);
                let (n, m) = (image.width() as f64, image.height() as f64);
                let px = (u * n - 0.5).floor().clamp(0.0, n - 1.0) as u32;
                let py = (v * m - 0.5).floor().clamp(0.0, m - 1.0) as u32;
                out.put_pixel(x as u32, y as u32, *image.get_pixel(px, py));
            }
        }
        Ok(out)
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
        let (u, v) = crate::geo::TileId::at(
            42.17745f64.to_radians(),
            42.48535f64.to_radians(),
            2,
        )
        .frac(42.17745f64.to_radians(), 42.48535f64.to_radians());
        let img = &world_start.imagery[0];
        let n = world_start.img_size as usize;
        let pixel = &img[((v * n as f64) as usize * n + (u * n as f64) as usize) * 4..][..4];
        assert_ne!(pixel, [63, 82, 61, 255], "coarse imagery should cover the package at its true location");
    }
}
