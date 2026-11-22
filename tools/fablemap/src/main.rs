use anyhow::{Context, Result, anyhow};
use flate2::read::GzDecoder;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use ureq::Agent;

const FORMAT: &str = "fablemap";
const FORMAT_VERSION: u32 = 1;
const DATA_ZOOM: u8 = 14;
const DEFAULT_IMAGERY: &str =
    "https://server.arcgisonline.com/ArcGIS/rest/services/World_Imagery/MapServer/tile/{z}/{y}/{x}";
const VECTOR_TILEJSON: &str = "https://tiles.openfreemap.org/planet";
const TERRARIUM: &str =
    "https://s3.amazonaws.com/elevation-tiles-prod/terrarium/{z}/{x}/{y}.png";
const COVERAGE_BUFFER_KM: f64 = 10.0;
const DATA_NEIGHBOR_TILES: i64 = 1;

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Manifest {
    format: String,
    format_version: u32,
    name: String,
    center: [f64; 2],
    radius_km: f64,
    bounds: [f64; 4],
    zoom: Zoom,
    sources: Sources,
    completed: bool,
    downloaded_bytes: u64,
    files: BTreeMap<String, FileRecord>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Zoom {
    imagery_min: u8,
    imagery_max: u8,
    data: u8,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Sources {
    imagery_template: String,
    elevation_template: String,
    vector_tilejson: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct FileRecord {
    kind: String,
    size: u64,
    sha256: String,
}

#[derive(Clone, Debug)]
struct Airport {
    ident: String,
    iata: String,
    lat: f64,
    lon: f64,
    name: String,
    city: String,
}

fn usage() -> &'static str {
    "fablemap create [--airport CODE | --lat DEG --lon DEG] --radius-km KM [--output DIR]\n\
     fablemap extend MAP_DIR [--radius-km KM]\n\
     fablemap info MAP_DIR\n\
     fablemap verify MAP_DIR\n\
\n\
create options:\n\
  --airport CODE       Resolve an airport from assets/airports.tsv.gz\n\
  --lat DEG --lon DEG  Map centre coordinates (both required)\n\
  --radius-km KM       Bounding radius in kilometres (default: 40)\n\
  --output DIR         Package directory (default: maps/<airport-or-coordinates>)\n\
  --min-zoom Z         Lowest imagery zoom (default: 8)\n\
  --max-zoom Z         Highest imagery zoom (default: 14)\n\
  --imagery-url URL    Imagery URL template\n\
\n\
extend options:\n\
  --radius-km KM       New radius around the package's existing centre"
}

fn arg(v: &[String], name: &str) -> Option<String> {
    v.iter().position(|a| a == name).and_then(|i| v.get(i + 1)).cloned()
}

fn flag(v: &[String], name: &str) -> bool {
    v.iter().any(|a| a == name)
}

fn main() -> Result<()> {
    let v: Vec<String> = std::env::args().skip(1).collect();
    if v.is_empty() || flag(&v, "--help") || flag(&v, "-h") {
        println!("{}", usage());
        return Ok(());
    }
    match v[0].as_str() {
        "create" => create(&v[1..]),
        "info" => {
            let path = v.get(1).map(PathBuf::from).context("usage: fablemap info MAP_DIR")?;
            info(&path)
        }
        "verify" => {
            let path = v.get(1).map(PathBuf::from).context("usage: fablemap verify MAP_DIR")?;
            verify(&path)
        }
        "extend" => extend(&v[1..]),
        _ => Err(anyhow!("unknown command '{}'\n\n{}", v[0], usage())),
    }
}

fn extend(v: &[String]) -> Result<()> {
    let path = v.first().map(PathBuf::from).context("usage: fablemap extend MAP_DIR [--radius-km KM]")?;
    let args = &v[1..];
    let manifest = load_manifest(&path)?;
    let radius = arg(args, "--radius-km").unwrap_or_else(|| (manifest.radius_km * 2.0).to_string()).parse::<f64>().context("invalid --radius-km")?;
    if !(radius > 0.0 && radius <= 5000.0) {
        return Err(anyhow!("--radius-km must be greater than 0 and at most 5000"));
    }
    if radius < manifest.radius_km {
        return Err(anyhow!("the new radius ({radius:.1} km) is smaller than the existing radius ({:.1} km)", manifest.radius_km));
    }
    let create_args = vec![
        "--lat".to_string(), manifest.center[0].to_string(),
        "--lon".to_string(), manifest.center[1].to_string(),
        "--radius-km".to_string(), radius.to_string(),
        "--output".to_string(), path.to_string_lossy().into_owned(),
        "--min-zoom".to_string(), manifest.zoom.imagery_min.to_string(),
        "--max-zoom".to_string(), manifest.zoom.imagery_max.to_string(),
        "--imagery-url".to_string(), manifest.sources.imagery_template.clone(),
    ];
    println!("Extending {} from {:.1} km to {:.1} km", path.display(), manifest.radius_km, radius);
    create(&create_args)
}

fn create(v: &[String]) -> Result<()> {
    let radius = arg(v, "--radius-km").unwrap_or_else(|| "40".into()).parse::<f64>().context("invalid --radius-km")?;
    if !(radius > 0.0 && radius <= 5000.0) {
        return Err(anyhow!("--radius-km must be greater than 0 and at most 5000"));
    }
    let min_zoom = arg(v, "--min-zoom").unwrap_or_else(|| "8".into()).parse::<u8>().context("invalid --min-zoom")?.clamp(2, 14);
    let max_zoom = arg(v, "--max-zoom").unwrap_or_else(|| "14".into()).parse::<u8>().context("invalid --max-zoom")?.clamp(min_zoom, 19);
    let imagery_template = arg(v, "--imagery-url").unwrap_or_else(|| DEFAULT_IMAGERY.into());

    let (lat, lon, label) = if let Some(code) = arg(v, "--airport") {
        let airport = find_airport(&code)?.ok_or_else(|| anyhow!("airport '{}' was not found", code))?;
        let label = if airport.iata.is_empty() { airport.ident.clone() } else { format!("{}-{}", airport.ident, airport.iata) };
        println!("Airport: {} — {}, {}, {} ({:.5}, {:.5})", label, airport.name, airport.city, airport.ident, airport.lat, airport.lon);
        (airport.lat, airport.lon, label)
    } else {
        let lat = arg(v, "--lat").context("provide --airport or both --lat and --lon")?.parse::<f64>().context("invalid --lat")?;
        let lon = arg(v, "--lon").context("provide --airport or both --lat and --lon")?.parse::<f64>().context("invalid --lon")?;
        if !lat.is_finite() || !lon.is_finite() || !(-90.0..=90.0).contains(&lat) || !(-180.0..=180.0).contains(&lon) {
            return Err(anyhow!("invalid coordinate; latitude must be -90..90 and longitude -180..180"));
        }
        let label = format!("{:.5}-{:.5}", lat, lon);
        (lat, lon, label)
    };

    let output = arg(v, "--output").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("maps").join(label.to_lowercase().replace(['/', '\\'], "-")));
    fs::create_dir_all(output.join("patches"))?;
    fs::create_dir_all(output.join("overlays"))?;

    let coverage_radius = radius + COVERAGE_BUFFER_KM;
    let bounds = geographic_bounds(lat, lon, coverage_radius);
    let mut manifest = match load_manifest(&output) {
        Ok(mut existing) => {
            if (existing.center[0] - lat).abs() > 1e-6 || (existing.center[1] - lon).abs() > 1e-6 {
                return Err(anyhow!(
                    "package centre ({:.6}, {:.6}) differs from requested centre ({:.6}, {:.6}); use a separate output to create another map",
                    existing.center[0], existing.center[1], lat, lon
                ));
            }
            existing.completed = false;
            existing.radius_km = radius;
            existing.bounds = bounds;
            existing.zoom.imagery_min = min_zoom.min(existing.zoom.imagery_min);
            existing.zoom.imagery_max = max_zoom.max(existing.zoom.imagery_max);
            existing.sources.imagery_template = imagery_template;
            existing
        }
        Err(_) => Manifest {
            format: FORMAT.into(),
            format_version: FORMAT_VERSION,
            name: label.clone(),
            center: [lat, lon],
            radius_km: radius,
            bounds,
            zoom: Zoom { imagery_min: min_zoom, imagery_max: max_zoom, data: DATA_ZOOM },
            sources: Sources {
                imagery_template: imagery_template,
                elevation_template: TERRARIUM.into(),
                vector_tilejson: VECTOR_TILEJSON.into(),
            },
            completed: false,
            downloaded_bytes: 0,
            files: BTreeMap::new(),
        },
    };
    manifest.completed = false;
    write_manifest(&output, &manifest)?;

    let agent = Agent::config_builder()
        .timeout_global(Some(std::time::Duration::from_secs(30)))
        .user_agent(concat!("fablemap/", env!("CARGO_PKG_VERSION")))
        .http_status_as_error(false)
        .build()
        .into();

    let mut failures = 0usize;

    for z in min_zoom..=max_zoom {
        let tiles = tiles_for_bounds(bounds, z);
        println!("Imagery z{}: {} tiles", z, tiles.len());
        let mut done = 0usize;
        let mut new_bytes = 0u64;
        let mut downloaded = 0usize;
        let mut cached = 0usize;
        for (x, y) in &tiles {
            let rel = format!("imagery/{z}/{x}/{y}.jpg");
            let url = template(&manifest.sources.imagery_template, z, *x, *y);
            match download_file(&agent, &output.join(&rel), &url) {
                Ok((recorded, len)) => {
                    if recorded {
                        new_bytes = new_bytes.saturating_add(len);
                        downloaded += 1;
                    } else {
                        cached += 1;
                    }
                    manifest.downloaded_bytes = downloaded_manifest_bytes(&manifest.files);
                    manifest.files.insert(rel.clone(), record_file(&output.join(&rel), "imagery")?);
                }
                Err(e) => {
                    eprintln!("  missing/failed {}: {}", rel, e);
                    failures += 1;
                }
            }
            done += 1;
            if done == tiles.len() || done % 25 == 0 {
                progress_line("imagery", z, done, tiles.len(), downloaded, new_bytes, cached);
            }
        }
        manifest.downloaded_bytes = downloaded_manifest_bytes(&manifest.files);
    }

    let elevation_tiles = tiles_for_bounds_with_margin(bounds, DATA_ZOOM, DATA_NEIGHBOR_TILES);
    println!("Elevation z{}: {} tiles", DATA_ZOOM, elevation_tiles.len());
    let mut done = 0usize;
    let mut new_bytes = 0u64;
    let mut downloaded = 0usize;
    let mut cached = 0usize;
    for (x, y) in elevation_tiles.clone() {
        let rel = format!("elevation/{DATA_ZOOM}/{x}/{y}.png");
        let url = template(&manifest.sources.elevation_template, DATA_ZOOM, x, y);
        match download_file(&agent, &output.join(&rel), &url) {
            Ok((recorded, len)) => {
                if recorded {
                    new_bytes = new_bytes.saturating_add(len);
                    downloaded += 1;
                } else {
                    cached += 1;
                }
                manifest.files.insert(rel.clone(), record_file(&output.join(&rel), "elevation")?);
            }
            Err(e) => {
                eprintln!("  missing/failed {}: {}", rel, e);
                failures += 1;
            }
        }
        done += 1;
        if done == elevation_tiles.len() || done % 25 == 0 {
            progress_line("elevation", DATA_ZOOM, done, elevation_tiles.len(), downloaded, new_bytes, cached);
        }
    }

    let vector_template = get_vector_template(&agent, &manifest.sources.vector_tilejson)?;
    println!("Vector z{}: {} tiles", DATA_ZOOM, elevation_tiles.len());
    let mut done = 0usize;
    let mut new_bytes = 0u64;
    let mut downloaded = 0usize;
    let mut cached = 0usize;
    for (x, y) in elevation_tiles.clone() {
        let rel = format!("vector/{DATA_ZOOM}/{x}/{y}.pbf");
        let url = template(&vector_template, DATA_ZOOM, x, y);
        match download_file(&agent, &output.join(&rel), &url) {
            Ok((recorded, len)) => {
                if recorded {
                    new_bytes = new_bytes.saturating_add(len);
                    downloaded += 1;
                } else {
                    cached += 1;
                }
                manifest.files.insert(rel.clone(), record_file(&output.join(&rel), "vector")?);
            }
            Err(e) => {
                eprintln!("  missing/failed {}: {}", rel, e);
                failures += 1;
            }
        }
        done += 1;
        if done == elevation_tiles.len() || done % 25 == 0 {
            progress_line("vector", DATA_ZOOM, done, elevation_tiles.len(), downloaded, new_bytes, cached);
        }
    }

    manifest.downloaded_bytes = downloaded_manifest_bytes(&manifest.files);
    manifest.completed = failures == 0;
    write_manifest(&output, &manifest)?;

    println!("Created {} ({} downloaded bytes; {} failures)", output.display(), manifest.downloaded_bytes, failures);
    if failures != 0 {
        Err(anyhow!("map creation completed with {} failed downloads; rerun create to resume", failures))
    } else {
        Ok(())
    }
}

fn info(path: &Path) -> Result<()> {
    let manifest = load_manifest(path)?;
    println!("{} {}: {}", manifest.format, manifest.format_version, manifest.name);
    println!("Centre: {:.6}, {:.6}", manifest.center[0], manifest.center[1]);
    println!("Radius: {:.1} km", manifest.radius_km);
    println!("Bounds: {:.6}, {:.6} to {:.6}, {:.6}", manifest.bounds[0], manifest.bounds[1], manifest.bounds[2], manifest.bounds[3]);
    println!("Imagery zoom: {}..{}", manifest.zoom.imagery_min, manifest.zoom.imagery_max);
    println!("Data zoom: {}", manifest.zoom.data);
    println!("Files: {}", manifest.files.len());
    println!("Downloaded: {} bytes", manifest.downloaded_bytes);
    println!("Complete: {}", manifest.completed);
    Ok(())
}

fn verify(path: &Path) -> Result<()> {
    let manifest = load_manifest(path)?;
    let mut missing = 0usize;
    let mut mismatched = 0usize;
    for (rel, record) in &manifest.files {
        let file = path.join(rel);
        match fs::read(&file) {
            Ok(data) => {
                let hash = sha256(&data);
                if data.len() as u64 != record.size {
                    mismatched += 1;
                } else if hash != record.sha256 {
                    mismatched += 1;
                }
            }
            Err(_) => {
                missing += 1;
            }
        }
    }
    println!("Verified {} manifest entries", manifest.files.len());
    println!("Missing: {}", missing);
    println!("Size/hash mismatches: {}", mismatched);
    if missing == 0 && mismatched == 0 {
        Ok(())
    } else {
        Err(anyhow!("verification failed"))
    }
}

fn load_manifest(path: &Path) -> Result<Manifest> {
    let data = fs::read(path.join("manifest.json")).with_context(|| format!("could not read {}", path.join("manifest.json").display()))?;
    let manifest: Manifest = serde_json::from_slice(&data).context("invalid map manifest")?;
    if manifest.format != FORMAT {
        return Err(anyhow!("not a fablemap package"));
    }
    if manifest.format_version != FORMAT_VERSION {
        return Err(anyhow!("unsupported map format version {}", manifest.format_version));
    }
    Ok(manifest)
}

fn write_manifest(path: &Path, manifest: &Manifest) -> Result<()> {
    let json = serde_json::to_vec_pretty(manifest)?;
    let tmp = path.join("manifest.json.tmp");
    let mut file = fs::File::create(&tmp)?;
    file.write_all(&json)?;
    file.sync_all()?;
    fs::rename(tmp, path.join("manifest.json"))?;
    Ok(())
}

fn record_file(path: &Path, kind: &str) -> Result<FileRecord> {
    let data = fs::read(path)?;
    Ok(FileRecord { kind: kind.into(), size: data.len() as u64, sha256: sha256(&data) })
}

fn sha256(data: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(data);
    format!("{:x}", h.finalize())
}

fn byte_count(bytes: u64) -> String {
    if bytes >= 1 << 30 {
        format!("{:.2} GiB", bytes as f64 / (1u64 << 30) as f64)
    } else if bytes >= 1 << 20 {
        format!("{:.1} MiB", bytes as f64 / (1u64 << 20) as f64)
    } else if bytes >= 1 << 10 {
        format!("{:.1} KiB", bytes as f64 / 1024.0)
    } else {
        format!("{bytes} B")
    }
}

fn progress_line(kind: &str, z: u8, done: usize, total: usize, downloaded: usize, new_bytes: u64, cached: usize) {
    let remaining = total.saturating_sub(done);
    print!(
        "\r  {} z{}: {done}/{total} tiles, {downloaded} downloaded ({}), {cached} cached, {remaining} remaining",
        kind,
        z,
        byte_count(new_bytes)
    );
    let _ = std::io::stdout().flush();
    if done == total {
        println!();
    }
}

fn downloaded_manifest_bytes(files: &BTreeMap<String, FileRecord>) -> u64 {
    files.values().map(|record| record.size).sum()
}

fn download_file(agent: &Agent, path: &Path, url: &str) -> Result<(bool, u64)> {
    if let Ok(data) = fs::read(path) {
        if !data.is_empty() {
            return Ok((false, data.len() as u64));
        }
        if path.extension().and_then(|e| e.to_str()) == Some("pbf") {
            return Ok((false, 0));
        }
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut response = agent.get(url).call().with_context(|| format!("GET {}", url))?;
    let status = response.status();
    if !(200..=299).contains(&status.as_u16()) {
        return Err(anyhow!("HTTP {}", status));
    }
    let data = response.body_mut().with_config().limit(64 << 20).read_to_vec()?;
    if data.is_empty() {
        fs::write(path, &data)?;
        return Ok((true, 0));
    }
    let tmp = path.with_extension(format!("tmp-{}", std::process::id()));
    fs::write(&tmp, &data)?;
    fs::rename(tmp, path)?;
    Ok((true, data.len() as u64))
}

fn get_vector_template(agent: &Agent, url: &str) -> Result<String> {
    let mut response = agent.get(url).call().with_context(|| format!("GET {}", url))?;
    let status = response.status();
    if !(200..=299).contains(&status.as_u16()) {
        return Err(anyhow!("HTTP {}", status));
    }
    let data = response.body_mut().with_config().limit(16 << 20).read_to_vec()?;
    let json: serde_json::Value = serde_json::from_slice(&data)?;
    json["tiles"][0].as_str().map(str::to_string).ok_or_else(|| anyhow!("TileJSON did not contain a tiles URL"))
}

fn template(template: &str, z: u8, x: u32, y: u32) -> String {
    template
        .replace("{z}", &z.to_string())
        .replace("{x}", &x.to_string())
        .replace("{y}", &y.to_string())
}

fn lat_to_merc_y(lat: f64) -> f64 {
    let lat = lat.to_radians().clamp(-1.484422229, 1.484422229);
    (1.0 - (lat.tan().asinh()) / std::f64::consts::PI) * 0.5
}

fn lon_to_merc_x(lon: f64) -> f64 {
    (lon.to_radians() + std::f64::consts::PI) / (2.0 * std::f64::consts::PI)
}

fn geographic_bounds(lat: f64, lon: f64, radius_km: f64) -> [f64; 4] {
    let dlat = radius_km / 111.195;
    let dlon = radius_km / (111.195 * lat.to_radians().cos().abs().max(0.05));
    [lat - dlat, lon - dlon, lat + dlat, lon + dlon]
}

fn tiles_for_bounds(bounds: [f64; 4], z: u8) -> Vec<(u32, u32)> {
    let n = 1u64 << z;
    let (south, west, north, east) = (bounds[0], bounds[1], bounds[2], bounds[3]);
    let x0 = (lon_to_merc_x(west.clamp(-180.0, 180.0)) * n as f64).floor() as i64;
    let x1 = (lon_to_merc_x(east.clamp(-180.0, 180.0)).rem_euclid(1.0) * n as f64).ceil() as i64;
    let y0 = (lat_to_merc_y(north) * n as f64).floor() as i64;
    let y1 = (lat_to_merc_y(south) * n as f64).ceil() as i64;

    let x0 = x0.clamp(0, n as i64 - 1) as u32;
    let x1 = x1.clamp(1, n as i64) as u32;
    let y0 = y0.clamp(0, n as i64 - 1) as u32;
    let y1 = y1.clamp(1, n as i64) as u32;

    let mut out = Vec::new();
    for y in y0..y1 {
        for x in x0..x1 {
            out.push((x, y));
        }
    }
    out
}

fn tiles_for_bounds_with_margin(bounds: [f64; 4], z: u8, margin: i64) -> Vec<(u32, u32)> {
    let out = tiles_for_bounds(bounds, z);
    let size = 1i64 << z;
    let mut expanded = std::collections::BTreeSet::new();
    for &(x, y) in &out {
        for dy in -margin..=margin {
            for dx in -margin..=margin {
                let x = (x as i64 + dx).rem_euclid(size);
                let y = y as i64 + dy;
                if y >= 0 && y < size {
                    expanded.insert((x as u32, y as u32));
                }
            }
        }
    }
    expanded.into_iter().collect()
}

fn airport_database_path() -> Result<PathBuf> {
    let candidates = [
        PathBuf::from("assets/airports.tsv.gz"),
        PathBuf::from("../../assets/airports.tsv.gz"),
        PathBuf::from("../../../assets/airports.tsv.gz"),
    ];
    candidates.into_iter().find(|p| p.exists()).ok_or_else(|| anyhow!("could not find assets/airports.tsv.gz"))
}

fn find_airport(code: &str) -> Result<Option<Airport>> {
    let path = airport_database_path()?;
    let file = fs::File::open(path)?;
    let mut decoder = GzDecoder::new(file);
    let mut text = String::new();
    decoder.read_to_string(&mut text)?;
    let code = code.trim().to_ascii_uppercase();
    for line in text.lines() {
        let fields: Vec<&str> = line.split('\t').collect();
        if fields.first() != Some(&"A") || fields.len() < 11 {
            continue;
        }
        let ident = fields[1].trim().to_ascii_uppercase();
        let iata = fields[2].trim().to_ascii_uppercase();
        if ident != code && iata != code {
            continue;
        }
        return Ok(Some(Airport {
            ident,
            iata,
            lat: fields[4].parse().context("invalid airport latitude")?,
            lon: fields[5].parse().context("invalid airport longitude")?,
            name: fields[7].trim().to_string(),
            city: fields[8].trim().to_string(),
        }));
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mercator_tile_range_is_nonempty() {
        let bounds = geographic_bounds(57.663, 14.069, 5.0);
        let tiles = tiles_for_bounds(bounds, 14);
        assert!(!tiles.is_empty());
    }

    #[test]
    fn airport_database_resolves_esgj() {
        let airport = find_airport("ESGJ").unwrap();
        assert!(airport.is_some());
    }

    #[test]
    fn template_replaces_xyz() {
        assert_eq!(template("https://x/{z}/{x}/{y}.jpg", 14, 3, 4), "https://x/14/3/4.jpg");
    }

    #[test]
    fn data_tiles_include_neighboring_border() {
        let bounds = geographic_bounds(42.17745, 42.48535, 60.0);
        let ordinary = tiles_for_bounds(bounds, DATA_ZOOM);
        let bordered = tiles_for_bounds_with_margin(bounds, DATA_ZOOM, DATA_NEIGHBOR_TILES);
        assert!(bordered.len() > ordinary.len());
    }
}
