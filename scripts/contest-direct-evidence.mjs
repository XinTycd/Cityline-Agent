import { archive, csvRow, routeKey, sampleMonth, verifiedReach } from './contest-http.mjs';

const evidencePath = '/api/media-proposals/platform-evidence';
const reachHeader = '媒体类型,资源编号或线路,统计月份,每日触达人次,计量单位,来源记录';
const observationMonth = `${sampleMonth.slice(0, 4)}-${sampleMonth.slice(4)}`;
const reachCsv = (rows) => [reachHeader + ',估算基准', ...rows.map(csvRow)].join('\r\n');

function thin(line, limit = 280) {
  const step = Math.max(1, Math.ceil((line.length - 1) / (limit - 1)));
  return line.filter((_, i) => i % step === 0 || i === line.length - 1);
}

async function bounded(work, action) {
  for (let i = 0; i < work.length; i += 4) {
    await Promise.all(work.slice(i, i + 4).map(action));
  }
}

function reachKey(media, value) {
  return media === 'bus' ? routeKey(value) : routeKey(value)
    .replace(/^地铁(?:(?:\d+号线|[\p{L}]+线)[-·]*)?/u, '')
    .replace(/站(?=\d|[a-z]|$)/u, '')
    .replace(/号?出入口$|号?出口$|号口$/u, '口');
}

function matchReachLabel(asset, labels) {
  const aliases = new Set([asset.name, asset.id].map((value) => reachKey(asset.media, value)));
  const exact = labels.filter((label) => aliases.has(reachKey(asset.media, label.thirdLabel)));
  if (exact.length === 1) return { label: exact[0], stationOnly: false };
  if (exact.length || asset.media !== 'metro' || !asset.station) return null;
  const station = reachKey('metro', asset.station);
  const nearby = labels.filter((label) => reachKey('metro', label.thirdLabel) === station);
  return nearby.length === 1 ? { label: nearby[0], stationOnly: true } : null;
}

async function saveEvidence(origin, headers, batch, field, filename, payload) {
  const body = new FormData();
  body.set('batch', batch);
  body.set(field, new Blob([payload]), filename);
  const saved = await fetch(origin + evidencePath, {
    headers, method: 'POST', body, signal: AbortSignal.timeout(20_000),
  });
  if (saved.status !== 204) throw new Error('The workbench refused the contest evidence');
}

async function readObservation(cloud, asset, labels, measures, failures) {
  const { media } = asset;
  const match = matchReachLabel(asset, labels);
  if (!match) return { state: 'resource-not-confirmed' };
  if ((failures.get(media) ?? 0) >= 3) return { state: 'paused-after-provider-errors' };
  try {
    const { label, stationOnly } = match;
    if (!measures.has(label.id)) measures.set(label.id, await cloud.reach(label.id));
    const data = measures.get(label.id);
    const readable = Array.isArray(data) ? data.filter((row) => row?.type === 'coverPeoplesPVAccurate') : [];
    const count = stationOnly ? null : verifiedReach(data, media);
    const regionalDaily = readable.length === 1 ? Number(readable[0].time1) : NaN;
    return {
      state: count ? 'verified' : 'unit-not-confirmed', count,
      reference: `新云媒体查询 ${observationMonth} ${label.thirdLabel} · 标签 ${label.id}`
        + (stationOnly ? ' · 站级历史人流' : ''),
      regionalDaily: Number.isFinite(regionalDaily) && regionalDaily >= 1 && regionalDaily <= 100_000_000
        ? Math.round(regionalDaily) : null,
    };
  } catch {
    failures.set(media, (failures.get(media) ?? 0) + 1);
    return { state: 'platform-error' };
  }
}

export async function acquireDirectEvidence(cloud, directory, origin, headers, onPeriod, capture) {
  const manifest = await fetch(origin + evidencePath, { headers, signal: AbortSignal.timeout(35_000) });
  if (!manifest.ok) throw new Error('The workbench refused to prepare a scoped evidence manifest');
  const plans = await manifest.json();
  const maps = new Map();
  const observations = new Map();
  const screenshots = new Map();
  const failures = new Map();
  const overview = {
    maps: 0, expectedMaps: plans.reduce((total, plan) => total + plan.routes.length, 0),
    reach: 0, expectedReach: plans.reduce((total, plan) => total + plan.assets.length, 0), observedReach: 0,
    unavailablePeriods: plans.filter((plan) => plan.available === false).length,
  };
  const catalog = await cloud.routeCatalog().catch(() => new Map());
  const labelGroups = new Map();
  const labelMeasures = new Map();

  for (const plan of plans) {
    await onPeriod(plan, overview);
    const routes = [...new Set(plan.routes.flat())];
    await bounded(routes, async (route) => {
      const key = routeKey(route);
      if (maps.has(key)) return;
      const path = catalog.get(key);
      const geometry = path ? await cloud.route(path).catch(() => null) : null;
      maps.set(key, geometry?.length >= 2 ? thin(geometry) : null);
    });

    const features = Array.from(new Map(routes.map((route) => [routeKey(route), route]))).flatMap(([key, resource]) => {
      const coordinates = maps.get(key);
      if (!coordinates) return [];
      return [{
        type: 'Feature',
        properties: { media: 'bus', resource, source_ref: '新云 AI 线路原始轨迹 · ' + resource + ' · 百度地图坐标系' },
        geometry: { type: 'LineString', coordinates },
      }];
    });
    if (features.length) {
      const data = JSON.stringify({ type: 'FeatureCollection', features });
      await archive(directory, plan.starts + '-新云原始线路.geojson', data);
      await saveEvidence(origin, headers, plan.batch, 'route_geometry', '新云接口线路轨迹.geojson', data);
    }
    for (const [index, selected] of plan.routes.entries()) {
      const key = selected.map(routeKey).sort().join('|');
      if (!selected.every((route) => maps.get(routeKey(route)))) continue;
      if (!screenshots.has(key)) {
        const image = await capture(selected, catalog, plan.starts + '-新云线路原图-' + (index + 1) + '.jpg')
          .catch(() => null);
        if (image) screenshots.set(key, image);
      }
      if (screenshots.has(key)) {
        const image = JSON.stringify([screenshots.get(key)]);
        await saveEvidence(origin, headers, plan.batch, 'official_maps', '新云AI线路原图.json', image);
        overview.maps += 1;
        await onPeriod(plan, overview);
      }
    }

    const savedRows = [];
    const observationsFor = (asset) => observations.get(asset.media + '|' + asset.id);
    // Keep already acquired observations if a later group hits an interruption.
    for (let index = 0; index < plan.assets.length; index += 4) {
      const assets = plan.assets.slice(index, index + 4);
      for (const asset of assets) {
        const { media, id } = asset;
        if (observationsFor(asset)) continue;
        if (!labelGroups.has(media)) labelGroups.set(media, await cloud.labels(media).catch(() => []));
        const observation = await readObservation(cloud, asset, labelGroups.get(media), labelMeasures, failures);
        observations.set(media + '|' + id, observation);
      }
      const rows = assets.flatMap((asset) => {
        const observation = observationsFor(asset);
        if (!observation.count && !observation.regionalDaily) return [];
        const bus = asset.media === 'bus';
        return [[bus ? '公交车身' : '地铁出入口', asset.id, observationMonth, observation.count ?? observation.regionalDaily,
          observation.count ? (bus ? '人次/辆/天' : '人次/点位/天') : '平台标签日均人次',
          observation.reference, observation.count ? `新云${observationMonth}基准` : '']];
      });
      if (!rows.length) continue;
      savedRows.push(...rows);
      await archive(directory, plan.starts + '-新云单位触达.csv', reachCsv(savedRows));
      await saveEvidence(origin, headers, plan.batch, 'reach', '新云接口触达与历史人流.csv', reachCsv(rows));
      overview.reach += assets.filter((asset) => observationsFor(asset).count).length;
      overview.observedReach += rows.length;
      await onPeriod(plan, overview);
    }
    const log = { starts: plan.starts, ends: plan.ends, sourceMonth: observationMonth,
      routeMatches: routes.map((route) => ({ route, acquired: Boolean(maps.get(routeKey(route))) })),
      screenshots: plan.routes.map((routes) => ({ routes, acquired: screenshots.has(routes.map(routeKey).sort().join('|')) })),
      resources: plan.assets.map((asset) => ({
        media: asset.media, resource: asset.id, ...observationsFor(asset),
      })),
    };
    await archive(directory, plan.starts + '-新云接口取证.json', JSON.stringify(log, null, 2));
  }
  return overview;
}
