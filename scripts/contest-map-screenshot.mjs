import { archive, cloudOrigin, cloudPath, routeKey } from './contest-http.mjs';
const captureViewport = { width: 2880, height: 2160 };
const routePadding = 70;
const tileTimeout = 10000;

export async function captureContestMap(context, directory, routes, catalog, filename) {
  const renderer = cloudPath('RENDERER');
  const geometryEndpoint = cloudPath('GEOMETRY');
  const sources = routes.map((route) => ({ route, source: catalog.get(routeKey(route)) }));
  if (sources.some(({ source }) => !source)) return null;
  const page = await context.newPage();
  try {
    await page.setViewportSize(captureViewport);
    const origin = cloudOrigin + cloudPath('MAP_SCRIPT');
    // Keep the provider's routes. Use the SDK's standard basemap in this
    // capture-only tab because the custom canvas layer can paint blank tiles.
    await page.route(origin, async (route) => {
      const res = await route.fetch({ timeout: 15000, maxRedirects: 0 });
      const script = await res.text();
      const marker = 'var map = new BMap.Map("allmap");';
      const trackLoadedView = `
        window.citylineMapViewport = map;
        map.setMapStyleV2 = function () {};
        map.addEventListener('tilesloaded', function () {
          const center = map.getCenter();
          const size = map.getSize();
          window.citylineLoadedView = {
            lng: center.lng, lat: center.lat, zoom: map.getZoom(), width: size.width, height: size.height
          };
        });
      `;
      const body = script
        .replace(marker, marker + trackLoadedView)
        .replace(/map\.addControl\(new BMap\.NavigationControl\(opts\)\);/u, '');
      await route.fulfill({ response: res, body });
    });
    const response = await page.goto(cloudOrigin + renderer, { waitUntil: 'domcontentloaded', timeout: 20000 });
    if (!response?.ok() || new URL(page.url()).pathname !== renderer) return null;
    await page.waitForFunction(() => typeof window.readFile === 'function'
      && typeof window.citylineMapViewport?.getOverlays === 'function', undefined, { timeout: 20000 });
    const expected = new Set(sources.map(({ source }) => source));
    if (expected.size !== sources.length) return null;
    const fetched = new Set();
    page.on('response', (reply) => {
      const url = new URL(reply.url());
      const path = url.searchParams.get('linePath');
      if (url.origin === cloudOrigin && url.pathname === geometryEndpoint && reply.ok() && expected.has(path)) {
        fetched.add(path);
      }
    });
    const data = await page.evaluate(async (input) => {
      const map = window.citylineMapViewport;
      const color = (index) => '#' + ['204fc9', '9149a4', '118172', 'bc502b', '58601d', 'b13464', '287aaf', '6746e2'][index % 8];
      for (let i = 0; i < input.length; i += 6) {
        const batch = input.slice(i, i + 6);
        batch.forEach((item) => window.readFile(item.source, color(input.indexOf(item))));
        const start = performance.now();
        while (true) {
          const overlays = map.getOverlays();
          if (batch.every(({ source }) => overlays.some((item) => item.linepath === source && typeof item.getPath === 'function'))) break;
          if (performance.now() - start > 12000) return null;
          await new Promise((resolve) => setTimeout(resolve, 120));
        }
      }
      const expected = new Set(input.map((item) => item.source));
      const overlays = map.getOverlays().filter((item) => expected.has(item.linepath) && typeof item.getPath === 'function');
      const paths = overlays.map((overlay) => overlay.getPath()).filter((points) => points.length >= 2);
      if (paths.length !== input.length) return null;
      const bounds = { west: Infinity, south: Infinity, east: -Infinity, north: -Infinity };
      for (const point of paths.flat()) {
        bounds.west = Math.min(bounds.west, point.lng);
        bounds.south = Math.min(bounds.south, point.lat);
        bounds.east = Math.max(bounds.east, point.lng);
        bounds.north = Math.max(bounds.north, point.lat);
      }
      if (Object.values(bounds).some((value) => !Number.isFinite(value))) return null;
      return bounds;
    }, sources);
    if (!data || fetched.size !== expected.size) return null;
    const fitted = await page.evaluate(({ bounds: { west, south, east, north }, padding }) => {
      const map = window.citylineMapViewport;
      const extent = [new window.BMap.Point(west, south), new window.BMap.Point(east, north)];
      if (typeof map.checkResize === 'function') map.checkResize();
      const view = map.getViewport(extent, { margins: [padding, padding, padding, padding] });
      map.centerAndZoom(view.center, view.zoom);
      const [first, last] = extent.map((point) => map.pointToPixel(point));
      const center = map.pixelToPoint(new window.BMap.Pixel(
        (first.x + last.x) / 2,
        (first.y + last.y) / 2,
      ));
      return {
        longitude: center.lng,
        latitude: center.lat,
        zoom: view.zoom,
        width: Math.max(840, Math.ceil(Math.abs(last.x - first.x) + padding * 2)),
        height: Math.max(520, Math.ceil(Math.abs(last.y - first.y) + padding * 2)),
      };
    }, { bounds: data, padding: routePadding });
    if (!Object.values(fitted).every(Number.isFinite) ||
        fitted.width > captureViewport.width || fitted.height > captureViewport.height) return null;
    // Resize the actual renderer around the selected routes before capturing.
    // Keep every route endpoint at the same zoom and avoid excess basemap.
    await page.setViewportSize({ width: fitted.width, height: fitted.height });
    await page.evaluate(({ longitude, latitude, zoom, paths }) => {
      const map = window.citylineMapViewport;
      if (typeof map.checkResize === 'function') map.checkResize();
      map.centerAndZoom(new window.BMap.Point(longitude, latitude), zoom);
      const wanted = new Set(paths);
      for (const overlay of map.getOverlays()) {
        if (wanted.has(overlay.linepath) && typeof overlay.setStrokeWeight === 'function') overlay.setStrokeWeight(4);
      }
    }, { ...fitted, paths: Array.from(expected) });
    const map = page.locator('#allmap');
    const style = `
      .float_div,
      #btnquanping,
      .c_button,
      #allmap .BMap_stdMpCtrl,
      #allmap [role="button"],
      #allmap button { display: none !important; }
      #allmap .BMapLabel { font-size: 22px !important; }
    `;
    await page.addStyleTag({ content: style });
    const ready = await page.waitForFunction(({ fitted, bounds, paths }) => {
      const map = window.citylineMapViewport;
      const size = map.getSize();
      const view = window.citylineLoadedView;
      if (!view || view.zoom !== fitted.zoom || view.width !== size.width || view.height !== size.height
        || Math.abs(view.lng - fitted.longitude) > 0.000001 || Math.abs(view.lat - fitted.latitude) > 0.000001) {
        return false;
      }
      const overlays = map.getOverlays();
      const live = new Set(overlays.filter((item) => typeof item.getPath === 'function'
        && item.getPath().length >= 2).map((item) => item.linepath));
      const extent = [new window.BMap.Point(bounds.west, bounds.south), new window.BMap.Point(bounds.east, bounds.north)];
      if (!paths.every((path) => live.has(path)) || !extent.every((point) => map.getBounds().containsPoint(point))) return false;

      const surface = document.getElementById('allmap');
      const frame = surface.getBoundingClientRect();
      const tiles = Array.from(surface.querySelectorAll('img')).flatMap(image => {
        if (!image.complete || image.naturalWidth < 128 || image.naturalHeight < 128) return [];
        const hostname = new URL(image.currentSrc, document.URL).hostname;
        if (!/^apimaponline\d+\.bdimg\.com$/u.test(hostname)) return [];
        const style = getComputedStyle(image);
        if (style.visibility !== 'visible' || Number(style.opacity) < .99) return [];
        const rect = image.getBoundingClientRect();
        return rect.width >= 128 && rect.height >= 128 ? [rect] : [];
      });
      const samples = [.05, .5, .95];
      return samples.every(column => samples.every(row => {
        const x = frame.left + frame.width * column;
        const y = frame.top + frame.height * row;
        return tiles.some(tile => x >= tile.left && x <= tile.right && y >= tile.top && y <= tile.bottom);
      }));
    }, { fitted, bounds: data, paths: Array.from(expected) }, { timeout: tileTimeout }).catch(() => null);
    if (!ready) return null;
    for (const quality of [80, 65, 48]) {
      const jpeg = await map.screenshot({ type: 'jpeg', quality, scale: 'css', timeout: 15000 });
      if (jpeg.length <= 440_000) {
        await archive(directory, filename, jpeg);
        return { routes, image: jpeg.toString('base64'), captured_at: new Date().toISOString(), basemap_verified: true };
      }
    }
    return null;
  } finally {
    await page.close();
  }
}
