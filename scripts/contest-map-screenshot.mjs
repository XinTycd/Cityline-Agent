import { archive, cloudOrigin, cloudPath, routeKey } from './contest-http.mjs';
const captureViewport = { width: 2880, height: 2160 };
const routePadding = 70;

export async function captureContestMap(context, directory, routes, catalog, filename) {
  const renderer = cloudPath('RENDERER');
  const geometryEndpoint = cloudPath('GEOMETRY');
  const sources = routes.map((route) => ({ route, source: catalog.get(routeKey(route)) }));
  if (sources.some(({ source }) => !source)) return null;
  const page = await context.newPage();
  try {
    await page.setViewportSize(captureViewport);
    const origin = cloudOrigin + cloudPath('MAP_SCRIPT');
    // Keep the official map and drawing functions. Expose the viewport and omit
    // the navigation widget in this capture-only browser tab.
    await page.route(origin, async (route) => {
      const res = await route.fetch({ timeout: 15000, maxRedirects: 0 });
      const script = await res.text();
      const marker = 'var map = new BMap.Map("allmap");';
      const body = script
        .replace(marker, marker + 'window.citylineMapViewport = map;')
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
    const containsRoutes = await page.evaluate(({ longitude, latitude, zoom, paths, bounds }) => {
      const map = window.citylineMapViewport;
      if (typeof map.checkResize === 'function') map.checkResize();
      map.centerAndZoom(new window.BMap.Point(longitude, latitude), zoom);
      const wanted = new Set(paths);
      for (const overlay of map.getOverlays()) {
        if (wanted.has(overlay.linepath) && typeof overlay.setStrokeWeight === 'function') overlay.setStrokeWeight(4);
      }
      const extent = [new window.BMap.Point(bounds.west, bounds.south), new window.BMap.Point(bounds.east, bounds.north)];
      return extent.every((point) => map.getBounds().containsPoint(point));
    }, { ...fitted, bounds: data, paths: Array.from(expected) });
    if (!containsRoutes) return null;
    const map = page.locator('#allmap');
    const style = `
      body > :not(#allmap),
      #allmap .BMap_stdMpCtrl,
      #allmap [role="button"],
      #allmap button { display: none !important; }
      #allmap .BMapLabel { font-size: 22px !important; }
    `;
    await page.addStyleTag({ content: style });
    await page.evaluate(() => new Promise((resolve) => {
      const map = window.citylineMapViewport;
      const settle = () => {
        map.removeEventListener('tilesloaded', settle);
        resolve();
      };
      map.addEventListener('tilesloaded', settle);
      setTimeout(settle, 2600);
    }));
    await page.waitForLoadState('networkidle', { timeout: 7000 }).catch(() => {});
    for (const quality of [80, 65, 48]) {
      const jpeg = await map.screenshot({ type: 'jpeg', quality, scale: 'css', timeout: 15000 });
      if (jpeg.length <= 440_000) {
        await archive(directory, filename, jpeg);
        return { routes, image: jpeg.toString('base64'), captured_at: new Date().toISOString() };
      }
    }
    return null;
  } finally {
    await page.close();
  }
}
