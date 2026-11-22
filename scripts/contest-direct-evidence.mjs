import { archive, csvRow, dailyCount, routeKey, sessionRejected, verifiedReach } from './contest-http.mjs';

const evidencePath = '/api/media-proposals/platform-evidence';
const reachHeader = '媒体类型,资源编号或线路,统计月份,每日触达人次,计量单位,来源记录';
const baselineMonth = '2023-05';
const reachCsv = (rows) => [reachHeader + ',估算基准', ...rows.map(csvRow)].join('\r\n');

function supportedMonths(plans) {
  const configured = process.env.CITYLINE_NEWCLOUD_REACH_MONTHS?.trim();
  const months = configured
    ? configured.split(',').map(value => value.trim())
    : [baselineMonth, ...plans.flatMap(plan => plan.months)];
  if (!months.every(month => /^2023-(0[1-9]|1[0-2])$/u.test(month))) {
    throw new Error('Invalid configured Newcloud observation months');
  }
  return new Set(months);
}

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

function labelError(error) {
  if (Number.isInteger(error?.status) && error.status >= 300 && error.status <= 599) {
    return { kind: 'http', status: error.status };
  }
  if (error?.name === 'TimeoutError' || error?.name === 'AbortError') return { kind: 'timeout' };
  if (error instanceof TypeError) return { kind: 'transport' };
  if (error instanceof SyntaxError) return { kind: 'invalid-json' };
  if (error?.message === 'Contest response exceeds size limit') return { kind: 'response-too-large' };
  return { kind: 'unexpected-response' };
}

async function loadLabels(cloud, media) {
  const failures = [];
  for (let attempt = 0; attempt < 2; attempt += 1) {
    try {
      return { labels: await cloud.labels(media), failures };
    } catch (error) {
      const details = labelError(error);
      failures.push(details);
      if (sessionRejected(error)) return { state: 'authorization-rejected', failures };
      const transient = [408, 500, 502, 503, 504].includes(details.status)
        || ['timeout', 'transport'].includes(details.kind);
      if (!transient || attempt === 1) break;
      await new Promise(resolve => setTimeout(resolve, 350));
    }
  }
  return { state: 'label-query-error', failures };
}

async function readObservation(cloud, asset, labels, month, measures, failures, denied) {
  const { media } = asset;
  const match = matchReachLabel(asset, labels);
  if (!match) return { state: 'resource-not-confirmed' };
  const scope = media + '|' + month;
  const identity = scope + '|' + match.label.id;
  if (!measures.has(identity)) {
    if (denied.has(media)) return { state: 'authorization-rejected' };
    if ((failures.get(scope) ?? 0) >= 3) return { state: 'paused-after-provider-errors' };
  }
  try {
    const { label, stationOnly } = match;
    if (!measures.has(identity)) measures.set(identity, await cloud.reach(label.id, month));
    const data = measures.get(identity);
    const readable = Array.isArray(data) ? data.filter((row) => row?.type === 'coverPeoplesPVAccurate') : [];
    const count = stationOnly ? null : verifiedReach(data, media);
    const regionalDaily = readable.length === 1 ? dailyCount(readable[0].time1) : null;
    return {
      state: count ? 'verified' : regionalDaily
        ? (stationOnly ? 'station-only' : 'unit-not-confirmed') : 'no-observation',
      count, regionalDaily,
      reference: `新云媒体查询 ${month} ${label.thirdLabel} · 标签 ${label.id}`
        + (stationOnly ? ' · 站级历史人流' : ''),
    };
  } catch (error) {
    if (sessionRejected(error)) {
      denied.add(media);
      return { state: 'authorization-rejected' };
    }
    if (error?.status === 429 || error?.status >= 500 || error?.name === 'TimeoutError') {
      failures.set(scope, (failures.get(scope) ?? 0) + 1);
    }
    return { state: 'platform-error' };
  }
}

export async function acquireDirectEvidence(cloud, directory, origin, headers, onPeriod, capture) {
  const manifest = await fetch(origin + evidencePath, { headers, signal: AbortSignal.timeout(120_000) });
  if (!manifest.ok) throw new Error('The workbench refused to prepare a scoped evidence manifest');
  const { periods: plans, overview_routes: overviewRoutes } = await manifest.json();
  const supported = supportedMonths(plans);
  const maps = new Map();
  const failures = new Map();
  const denied = new Set();
  const reachChecks = [];
  const metroAssets = plans.flatMap((plan) => plan.assets.filter((asset) => asset.media === 'metro'));
  const overview = {
    reach: 0, expectedReach: plans.reduce((total, plan) => total + plan.assets.length * plan.months.length, 0),
    observedReach: 0, baselineReach: 0,
    exits: metroAssets.filter((asset) => asset.exit_verified).length, expectedExits: metroAssets.length,
    overviewMaps: 0, expectedOverviewMaps: overviewRoutes.length,
    unavailablePeriods: plans.filter((plan) => plan.available === false).length,
  };
  const needRoutes = overviewRoutes.length > 0 || plans.some(plan => plan.geometry_routes.length > 0);
  const catalog = needRoutes ? await cloud.routeCatalog().catch(() => new Map()) : new Map();
  const labelGroups = new Map();
  const labelMeasures = new Map();
  const annualMaps = [];
  if (overviewRoutes.length > 0) await onPeriod(null, overview, reachChecks);
  for (const [index, routes] of overviewRoutes.entries()) {
    const image = await capture(routes, catalog, '新云投放线路总图-' + (index + 1) + '.jpg').catch(() => null);
    if (image) {
      annualMaps.push(image);
      await archive(directory, 'annual-maps.json', JSON.stringify(annualMaps));
      overview.overviewMaps += 1;
    }
    await onPeriod(null, overview, reachChecks);
  }

  for (const plan of plans) {
    await onPeriod(plan, overview, reachChecks);
    const routes = plan.geometry_routes;
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
    const savedRows = [];
    const resources = [];
    // Keep already acquired observations if a later group hits an interruption.
    for (let index = 0; index < plan.assets.length; index += 4) {
      const assets = plan.assets.slice(index, index + 4);
      const rows = [];
      for (const asset of assets) {
        const { media, id } = asset;
        const savedExact = new Set(asset.exact_reach);
        const savedBaseline = new Set(asset.baseline_reach);
        const pending = plan.months.filter((month) => !savedExact.has(month));
        const exactMonths = pending.filter((month) => supported.has(month));
        const lookup = async (month) => {
          if (!labelGroups.has(media)) {
            labelGroups.set(media, await loadLabels(cloud, media));
          }
          const group = labelGroups.get(media);
          return group.labels ? readObservation(cloud, asset, group.labels, month, labelMeasures, failures, denied) : group;
        };
        const observations = new Map();
        for (const month of exactMonths) {
          observations.set(month, await lookup(month));
        }
        const missingExact = pending.filter((month) => !observations.get(month)?.count);
        if (missingExact.some((month) => !savedBaseline.has(month))
          && supported.has(baselineMonth) && !observations.has(baselineMonth)) {
          observations.set(baselineMonth, await lookup(baselineMonth));
        }
        const baseline = observations.get(baselineMonth);
        for (const month of plan.months) {
          const exact = observations.get(month);
          const confirmed = savedExact.has(month) || Boolean(exact?.count);
          const estimated = !confirmed && (savedBaseline.has(month) || Boolean(baseline?.count));
          if (confirmed || estimated) {
            overview.reach++;
            if (estimated) overview.baselineReach++;
          }
          if (confirmed || estimated || exact?.regionalDaily || baseline?.regionalDaily) {
            overview.observedReach++;
          }
          if (!confirmed && !estimated) {
            reachChecks.push({ starts: plan.starts, media, resource: id, month: Number(month.slice(-2)),
              exact: exact?.state ?? 'month-not-configured',
              baseline: month !== baselineMonth ? baseline?.state ?? null : null });
          }
        }
        const bus = media === 'bus';
        for (const [month, observation] of observations) {
          if (!observation.count && !observation.regionalDaily) continue;
          const canUseBaseline = month === baselineMonth && observation.count && missingExact.length > 0;
          rows.push([bus ? '公交车身' : '地铁出入口', id, month, observation.count ?? observation.regionalDaily,
            observation.count ? (bus ? '人次/辆/天' : '人次/点位/天') : '平台标签日均人次',
            observation.reference, canUseBaseline ? `新云${baselineMonth}基准` : '']);
        }
        resources.push({ media, resource: id, observations: Object.fromEntries(observations),
          existingExact: [...savedExact], existingBaseline: [...savedBaseline],
          ...(media === 'metro' ? { exitVerified: asset.exit_verified === true } : {}),
        });
      }
      if (rows.length) {
        savedRows.push(...rows);
        await archive(directory, plan.starts + '-新云单位触达.csv', reachCsv(savedRows));
        await saveEvidence(origin, headers, plan.batch, 'reach', '新云接口触达与历史人流.csv', reachCsv(rows));
      }
      await onPeriod(plan, overview, reachChecks);
    }
    const log = { starts: plan.starts, ends: plan.ends, requestedMonths: plan.months,
      routeMatches: routes.map((route) => ({ route, acquired: Boolean(maps.get(routeKey(route))) })),
      labelQueries: Array.from(labelGroups, ([media, result]) => ({
        media, count: result.labels?.length ?? 0, state: result.state ?? 'ready', failures: result.failures,
      })),
      resources,
    };
    await archive(directory, plan.starts + '-新云接口取证.json', JSON.stringify(log, null, 2));
  }
  return { overview, reachChecks };
}
