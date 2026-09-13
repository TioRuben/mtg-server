#!/usr/bin/env python3
"""Sync a rolling MTG visible-image archive over Iberia.

Steps:
  1. Get an access token from the EUMETSAT Data Store API.
  2. Search recent FDHSI (EO:EUM:DAT:0662) and HRFI (EO:EUM:DAT:0665) products.
  3. Pair repeat cycles with the same sensing start and keep only the last 6 hours.
  4. Skip nighttime frames by checking solar elevation over Iberia.
  5. Download missing chunk subsets, render PNGs, update manifest/latest files, and prune old frames.

Credentials are read from EUMETSAT_CONSUMER_KEY / EUMETSAT_CONSUMER_SECRET.
"""

from __future__ import annotations

import argparse
import base64
import json
import math
import os
import re
import shutil
import sys
import tempfile
import time
import urllib.parse
import urllib.request
import warnings
from dataclasses import dataclass
from datetime import UTC, datetime, timedelta
from pathlib import Path

API_BASE = "https://api.eumetsat.int"
COLLECTION_FDHSI = "EO:EUM:DAT:0662"
COLLECTION_HRFI = "EO:EUM:DAT:0665"
CHUNKS = tuple(range(31, 38))
IBERIA_EXTENT = (-10.0, 35.0, 5.0, 44.5)
IBERIA_CENTER = (39.75, -2.5)
SEARCH_COUNT = 72
HISTORY_HOURS = 6
RESOLUTION_DEG = 0.005
DAYLIGHT_ELEVATION_DEG = 0.0
CHUNK_RE = re.compile(r"CHK-BODY.*_(\d{4})\.nc$")


def log(message: str) -> None:
    print(message, file=sys.stderr, flush=True)


def retrying(operation, *, attempts: int = 5, delay: float = 2.0):
    for attempt in range(attempts):
        try:
            return operation()
        except urllib.error.HTTPError as error:
            if error.code not in (429, 500, 502, 503, 504) or attempt == attempts - 1:
                raise
            wait = delay * (2**attempt)
            log(f"HTTP {error.code}, retrying in {wait:.0f}s")
            time.sleep(wait)
        except (urllib.error.URLError, TimeoutError) as error:
            if attempt == attempts - 1:
                raise
            wait = delay * (2**attempt)
            log(f"{error!r}, retrying in {wait:.0f}s")
            time.sleep(wait)


def http_json(url: str, *, headers: dict[str, str] | None = None, data: bytes | None = None) -> dict:
    def call() -> dict:
        request = urllib.request.Request(url, data=data, headers=headers or {})
        with urllib.request.urlopen(request, timeout=60) as response:
            return json.load(response)

    return retrying(call)


def get_token(key: str, secret: str) -> str:
    auth = base64.b64encode(f"{key}:{secret}".encode()).decode()
    payload = http_json(
        f"{API_BASE}/token",
        headers={
            "Authorization": f"Basic {auth}",
            "Content-Type": "application/x-www-form-urlencoded",
        },
        data=b"grant_type=client_credentials",
    )
    return payload["access_token"]


@dataclass(frozen=True)
class Product:
    collection: str
    identifier: str
    start: str
    entries: list[str]


@dataclass(frozen=True)
class ProductPair:
    start: str
    fdhsi: Product
    hrfi: Product


def parse_timestamp(value: str) -> datetime:
    return datetime.strptime(value, "%Y-%m-%dT%H:%M:%SZ").replace(tzinfo=UTC)


def frame_id_for(start: str) -> str:
    return start.replace("-", "").replace(":", "")


def solar_elevation_deg(when: datetime, latitude: float, longitude: float) -> float:
    day_of_year = when.timetuple().tm_yday
    minutes = when.hour * 60 + when.minute + when.second / 60
    gamma = 2 * math.pi / 365 * (day_of_year - 1 + (minutes / 60 - 12) / 24)

    declination = (
        0.006918
        - 0.399912 * math.cos(gamma)
        + 0.070257 * math.sin(gamma)
        - 0.006758 * math.cos(2 * gamma)
        + 0.000907 * math.sin(2 * gamma)
        - 0.002697 * math.cos(3 * gamma)
        + 0.00148 * math.sin(3 * gamma)
    )
    equation_of_time = 229.18 * (
        0.000075
        + 0.001868 * math.cos(gamma)
        - 0.032077 * math.sin(gamma)
        - 0.014615 * math.cos(2 * gamma)
        - 0.040849 * math.sin(2 * gamma)
    )
    true_solar_minutes = (minutes + equation_of_time + 4 * longitude) % 1440
    hour_angle = math.radians(true_solar_minutes / 4 - 180)
    latitude_rad = math.radians(latitude)
    cosine_zenith = (
        math.sin(latitude_rad) * math.sin(declination)
        + math.cos(latitude_rad) * math.cos(declination) * math.cos(hour_angle)
    )
    cosine_zenith = min(1.0, max(-1.0, cosine_zenith))
    return 90.0 - math.degrees(math.acos(cosine_zenith))


def is_daylight(start: str) -> bool:
    return solar_elevation_deg(parse_timestamp(start), *IBERIA_CENTER) > DAYLIGHT_ELEVATION_DEG


def search_latest(collection: str, count: int = SEARCH_COUNT) -> list[dict]:
    query = urllib.parse.urlencode(
        {"format": "json", "pi": collection, "sort": "start,time,0", "c": count}
    )
    payload = http_json(f"{API_BASE}/data/search-products/os?{query}")
    return payload.get("features", [])


def wanted_entries(feature: dict) -> list[str]:
    names = []
    for link in feature["properties"]["links"].get("sip-entries", []):
        title = link.get("title", "")
        match = CHUNK_RE.search(title)
        if match and int(match.group(1)) in CHUNKS:
            names.append(title)
    return sorted(names)


def by_start(features: list[dict]) -> dict[str, dict]:
    return {feature["properties"]["date"].split("/")[0]: feature for feature in features}


def to_product(collection: str, start: str, feature: dict) -> Product:
    entries = wanted_entries(feature)
    if len(entries) != len(CHUNKS):
        raise RuntimeError(f"{collection}: expected {len(CHUNKS)} chunks, found {len(entries)}")
    return Product(
        collection=collection,
        identifier=feature["properties"]["identifier"],
        start=start,
        entries=entries,
    )


def recent_pairs(now: datetime, history_hours: int) -> list[ProductPair]:
    log("querying recent FDHSI and HRFI product listings from EUMETSAT Data Store...")
    fdhsi = by_start(search_latest(COLLECTION_FDHSI))
    hrfi = by_start(search_latest(COLLECTION_HRFI))
    cutoff = now - timedelta(hours=history_hours)

    starts = sorted(set(fdhsi) & set(hrfi), reverse=True)
    log(f"found {len(starts)} matching FDHSI/HRFI observation cycle(s)")
    pairs = []
    night_count = 0
    for start in starts:
        when = parse_timestamp(start)
        if when < cutoff:
            continue
        if not is_daylight(start):
            night_count += 1
            continue
        pairs.append(
            ProductPair(
                start=start,
                fdhsi=to_product(COLLECTION_FDHSI, start, fdhsi[start]),
                hrfi=to_product(COLLECTION_HRFI, start, hrfi[start]),
            )
        )
    if night_count > 0:
        log(f"skipped {night_count} nighttime cycle(s) over Iberia")
    return pairs


def load_manifest(path: Path) -> dict[str, dict]:
    if not path.exists():
        return {}
    try:
        payload = json.loads(path.read_text())
    except json.JSONDecodeError:
        return {}
    frames = payload.get("frames", [])
    return {frame["id"]: frame for frame in frames if isinstance(frame, dict) and "id" in frame}


def write_json_atomic(path: Path, payload: dict) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_suffix(path.suffix + ".tmp")
    temporary.write_text(json.dumps(payload))
    temporary.replace(path)


def sync_latest_files(frame_path: Path, latest_image: Path, latest_metadata: Path, metadata: dict) -> None:
    latest_image.parent.mkdir(parents=True, exist_ok=True)
    shutil.copyfile(frame_path, latest_image.with_suffix(".tmp.png"))
    latest_image.with_suffix(".tmp.png").replace(latest_image)
    write_json_atomic(latest_metadata, metadata)


def delete_if_exists(path: Path) -> None:
    try:
        path.unlink()
    except FileNotFoundError:
        pass


def clear_latest(latest_image: Path, latest_metadata: Path) -> None:
    delete_if_exists(latest_image)
    delete_if_exists(latest_metadata)


def download_product(product: Product, token: str, workdir: Path) -> list[Path]:
    base = (
        f"{API_BASE}/data/download/1.0.0/collections/"
        f"{urllib.parse.quote(product.collection, safe='')}/products/"
        f"{urllib.parse.quote(product.identifier, safe='')}/entry"
    )
    paths = []
    total = len(product.entries)
    for idx, name in enumerate(product.entries, start=1):
        destination = workdir / name
        paths.append(destination)
        if destination.exists() and destination.stat().st_size > 0:
            log(f"  [{product.collection}] chunk {idx}/{total}: {name} (cached in temp)")
            continue
        url = f"{base}?name={urllib.parse.quote(name, safe='')}"
        log(f"  [{product.collection}] downloading chunk {idx}/{total}: {name}")

        def fetch() -> None:
            request = urllib.request.Request(
                url,
                headers={"Authorization": "Bearer " + token},
            )
            with urllib.request.urlopen(request, timeout=600) as response:
                temporary = destination.with_suffix(".part")
                with open(temporary, "wb") as handle:
                    shutil.copyfileobj(response, handle, length=1 << 20)
                temporary.replace(destination)

        retrying(fetch)
    return paths


def compose(files: list[Path], output: Path) -> str:
    import numpy as np
    import xarray as xr
    from pyresample import create_area_def
    from satpy import DataQuery, Scene
    from satpy.writers import get_enhanced_image
    from trollimage.xrimage import XRImage

    warnings.filterwarnings("ignore")

    grid_height = 1500
    grid_width = int(grid_height * 1.2)

    area = create_area_def(
        "iberia_square",
        {"proj": "longlat", "datum": "WGS84"},
        area_extent=IBERIA_EXTENT,
        shape=(grid_height, grid_width),
    )

    vis_low = DataQuery(name="vis_06", resolution=1000)
    vis_high = DataQuery(name="vis_06", resolution=500)
    scene = Scene(filenames=[str(path) for path in files], reader="fci_l1c_nc")

    composite = "true_color"
    try:
        scene.load([composite, vis_low, vis_high], generate=False)
    except Exception as error:
        log(f"  [compose] true_color failed ({error!r}); falling back to raw variant")
        composite = "true_color_raw_with_corrected_green"
        scene.load([composite, vis_low, vis_high], generate=False)

    log("  [compose] resampling to Iberia grid (1500x1800)...")
    local = scene.resample(area, resampler="nearest")

    log("  [compose] generating enhanced true-color base image...")
    rgb_img = get_enhanced_image(local[composite])
    rgb_data = rgb_img.data.compute().values
    high = local[vis_high].compute().values
    low = local[vis_low].compute().values

    log("  [compose] applying high-resolution sharpening...")
    with np.errstate(divide="ignore", invalid="ignore"):
        ratio = np.where((low > 0.05) & np.isfinite(high), high / low, 1.0)
    ratio = np.clip(ratio, 0.6, 1.4)
    sharpened_data = np.clip(rgb_data * ratio[np.newaxis, :, :], 0.0, 1.0)

    log("  [compose] rendering PNG...")
    final_xr = xr.DataArray(
        sharpened_data,
        dims=rgb_img.data.dims,
        coords=rgb_img.data.coords,
    )

    final_image = XRImage(final_xr)
    temporary = output.with_suffix(".tmp.png")
    final_image.save(str(temporary))
    temporary.replace(output)

    start_time = scene.start_time
    return start_time.strftime("%Y-%m-%dT%H:%M:%SZ") if start_time else ""


def generate_frame(pair: ProductPair, token: str, output: Path) -> dict:
    frame_id = frame_id_for(pair.start)
    workdir = Path(tempfile.mkdtemp(prefix=f"mtg-{frame_id}-"))
    try:
        log(f"  downloading 2x{len(CHUNKS)} NetCDF chunks for frame {frame_id}...")
        files = download_product(pair.fdhsi, token, workdir)
        files += download_product(pair.hrfi, token, workdir)
        output.parent.mkdir(parents=True, exist_ok=True)
        satellite_time = compose(files, output) or pair.start
    finally:
        shutil.rmtree(workdir, ignore_errors=True)

    return {
        "id": frame_id,
        "generated_unix": int(time.time()),
        "satellite_time": satellite_time,
        "product_id": pair.fdhsi.identifier,
    }


def sync_archive(
    *,
    token: str,
    archive_dir: Path,
    manifest_path: Path,
    latest_image: Path,
    latest_metadata: Path,
    history_hours: int,
) -> None:
    archive_dir.mkdir(parents=True, exist_ok=True)
    existing = load_manifest(manifest_path)
    pairs = recent_pairs(datetime.now(UTC), history_hours)

    missing_pairs = [
        pair
        for pair in pairs
        if not (
            existing.get(frame_id_for(pair.start))
            and (archive_dir / f"{frame_id_for(pair.start)}.png").exists()
        )
    ]
    cached_count = len(pairs) - len(missing_pairs)
    log(
        f"found {len(pairs)} daylight observation cycle(s) in the last {history_hours}h "
        f"({cached_count} already cached, {len(missing_pairs)} to download/process)"
    )

    keep: dict[str, dict] = {}
    downloaded_count = 0

    for index, pair in enumerate(pairs):
        frame_id = frame_id_for(pair.start)
        frame_path = archive_dir / f"{frame_id}.png"
        metadata = existing.get(frame_id)

        if metadata and frame_path.exists():
            keep[frame_id] = metadata
        else:
            downloaded_count += 1
            log(f"[{downloaded_count}/{len(missing_pairs)}] generating frame {frame_id} (observation: {pair.start})...")
            metadata = generate_frame(pair, token, frame_path)
            keep[frame_id] = metadata
            log(f"[{downloaded_count}/{len(missing_pairs)}] completed frame {frame_id} (satellite time: {metadata['satellite_time']})")

        if index == 0:
            sync_latest_files(frame_path, latest_image, latest_metadata, keep[frame_id])

    if not pairs:
        log("no daylight frames available within archive window; clearing latest")
        clear_latest(latest_image, latest_metadata)

    ordered_frames = sorted(
        keep.values(),
        key=lambda frame: frame["satellite_time"],
    )
    write_json_atomic(manifest_path, {"frames": ordered_frames})

    keep_ids = {frame["id"] for frame in ordered_frames}
    pruned_count = 0
    for image_path in archive_dir.glob("*.png"):
        if image_path.stem not in keep_ids:
            image_path.unlink(missing_ok=True)
            pruned_count += 1

    if pruned_count > 0:
        log(f"pruned {pruned_count} old frame(s) outside {history_hours}h window")

    log(
        f"archive sync finished: {downloaded_count} new frame(s) downloaded, "
        f"{len(ordered_frames)} active frame(s) in archive"
    )


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, required=True, help="Destination PNG for the latest frame")
    parser.add_argument("--metadata", type=Path, required=True, help="Destination JSON for the latest frame")
    parser.add_argument("--manifest", type=Path, required=True, help="Destination JSON manifest for the archive")
    parser.add_argument("--archive-dir", type=Path, required=True, help="Directory used for cached archive PNGs")
    parser.add_argument(
        "--history-hours",
        type=int,
        default=HISTORY_HOURS,
        help="How many hours of visible frames to keep",
    )
    args = parser.parse_args()

    key = os.environ.get("EUMETSAT_CONSUMER_KEY")
    secret = os.environ.get("EUMETSAT_CONSUMER_SECRET")
    if not key or not secret:
        log("EUMETSAT_CONSUMER_KEY / EUMETSAT_CONSUMER_SECRET are required")
        return 2

    log("requesting access token")
    token = get_token(key, secret)
    log("synchronizing recent visible repeat cycles")
    sync_archive(
        token=token,
        archive_dir=args.archive_dir,
        manifest_path=args.manifest,
        latest_image=args.output,
        latest_metadata=args.metadata,
        history_hours=args.history_hours,
    )
    log("done")
    return 0


if __name__ == "__main__":
    sys.exit(main())
