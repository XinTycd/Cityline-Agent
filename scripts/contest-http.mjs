import { writeFile } from 'node:fs/promises';
import { join } from 'node:path';

function configuredOrigin(name) {
  const value = process.env[name];
  if (!value) return null;
  const url = new URL(value);
  if (url.protocol !== 'https:' || url.username || url.password || url.search || url.hash || url.pathname !== '/') {
    throw new Error('Invalid configured platform origin');
  }
  return url.origin;
}

function configuredPath(provider, key) {
  const value = process.env[`CITYLINE_${provider}_${key}`];
  if (!/^\/(?!\/)[A-Za-z0-9_/.\-]*$/u.test(value ?? '') || value.split('/').some(part => part === '..' || part === '.')) {
    throw new Error('Invalid or missing configured platform path: ' + key);
  }
  return value;
}

export const cloudOrigin = configuredOrigin('CITYLINE_NEWCLOUD_ORIGIN');
export const redOrigin = configuredOrigin('CITYLINE_REDBEAN_ORIGIN');
export const cloudPath = (key) => configuredPath('NEWCLOUD', key);
export const redPath = (key) => configuredPath('REDBEAN', key);
const maxBytes = 2 * 1024 * 1024;
const xlsxMime = 'application/vnd.openxmlformats-officedocument.spreadsheetml.sheet';
const contestCenters = new Map(JSON.parse(process.env.CITYLINE_NEWCLOUD_CENTER_COORDINATES || '[]')
  .map(([name, point]) => {
    const verified = Array.isArray(point) && coordinate(...point);
    if (typeof name !== 'string' || !verified) throw new Error('Invalid configured coverage center');
    return [name, verified];
  }));

export function csvRow(values) {
  return values.map((value) => '"' + String(value ?? '').replaceAll('"', '""') + '"').join(',');
}

export function routeKey(value) {
  return String(value ?? '').normalize('NFKC').replace(/\s/gu, '').replace(/(\d)路$/u, '$1').toLowerCase();
}

function rejected(status) {
  return Object.assign(new Error('Contest service returned HTTP ' + status), { status });
}

export function sessionRejected(error) {
  return error?.status === 401 || error?.status === 403
    || (error?.status >= 300 && error?.status < 400);
}

async function request(origin, path, { headers, query, json, form, referer, excel = false }) {
  const url = new URL(path, origin);
  if (url.origin !== origin) throw new Error('Unexpected contest endpoint origin');
  for (const [key, value] of Object.entries(query ?? {})) url.searchParams.set(key, value);
  const options = { redirect: 'manual', signal: AbortSignal.timeout(22_000),
    headers: { Accept: excel ? xlsxMime + ',application/octet-stream' : 'application/json', ...headers } };
  if (referer) options.headers.Referer = origin + referer;
  if (json || form) {
    options.method = 'POST';
    options.headers['Content-Type'] = json ? 'application/json' : 'application/x-www-form-urlencoded; charset=UTF-8';
    options.headers.Origin = origin;
    options.body = json ? JSON.stringify(json) : new URLSearchParams(form);
  }
  const res = await fetch(url, options);
  if (!res.ok) throw rejected(res.status);
  if (Number(res.headers.get('content-length')) > maxBytes) throw new Error('Contest response exceeds size limit');
  if (!res.body) throw new Error('Empty contest response');
  const blocks = [];
  let size = 0;
  for await (const chunk of res.body) {
    size += chunk.byteLength;
    if (size > maxBytes) throw new Error('Contest response exceeds size limit');
    blocks.push(chunk);
  }
  const bytes = Buffer.concat(blocks);
  if (excel) {
    if (bytes.length < 4 || bytes.readUInt32LE(0) !== 0x04034b50) throw new Error('Official Excel export was not returned');
    return bytes;
  }
  const source = bytes.toString('utf8').replace(/^\ufeff/u, '');
  if (source.trimStart().startsWith('<')) throw rejected(401);
  const parsed = JSON.parse(source);
  if (!Array.isArray(parsed) && ![0, 200, '0000', '0', '200'].includes(parsed?.code)) {
    if ([401, 403].includes(Number(parsed?.code))) {
      throw rejected(Number(parsed.code));
    }
    throw new Error('Contest service rejected the request');
  }
  return Array.isArray(parsed) ? parsed : parsed.data;
}

function coordinate(lng, lat) {
  const a = Number(lng);
  const b = Number(lat);
  if (a >= 120.7 && a <= 122.1 && b >= 30.6 && b <= 32) return [a, b];
  return null;
}

const coverageColumns = '中心点名称,中心点经度,中心点纬度,覆盖半径,媒体类型,媒体名称,最近距离,最近站点,站点距离,站点经度,站点纬度,最近轨迹点经度,最近轨迹点纬度';

export function coverageCsv(rows, centers, radius) {
  const requested = new Set(centers.map((center) => center.name));
  const verified = new Set();
  const output = [ '明细分析', coverageColumns ];
  for (const row of rows) {
    if (!requested.has(row.centerName) || Number(row.radius) !== radius
      || !['bus', 'metro'].includes(row.mediaType) || !row.lineName) continue;
    if (!coordinate(row.centerLng, row.centerLat)
      || row.minDistance == null || !Number.isFinite(Number(row.minDistance))
      || Number(row.minDistance) < 0 || Number(row.minDistance) > radius) {
      throw new Error('Official coverage contains an invalid distance or center');
    }
    verified.add(row.centerName);
    const distance = row.nearestStaDistance == null ? '' : Math.round(row.nearestStaDistance) + '米';
    output.push(csvRow([row.centerName, row.centerLng, row.centerLat, radius + '米',
      row.mediaType === 'bus' ? '公交线路' : '地铁出入口', row.lineName, Math.round(row.minDistance) + '米',
      row.nearestStaName, distance, row.nearestStaLng, row.nearestStaLat, row.nearestLng, row.nearestLat]));
  }
  if (verified.size !== requested.size) throw new Error('At least one confirmed center has no coverage data');
  return Buffer.from('\ufeff' + output.join('\r\n') + '\r\n', 'utf8');
}

export function normalizePath(data) {
  const raw = typeof data === 'string' ? JSON.parse(data) : data;
  if (!Array.isArray(raw?.path) || raw.path.length < 2) return null;
  const line = raw.path.map((point) => coordinate(point.lng, point.lat) ?? coordinate(point.lat, point.lng));
  if (line.some((point) => point === null)) return null;
  return line.filter((point, index) => !index || point[0] !== line[index - 1][0] || point[1] !== line[index - 1][1]);
}

export function dailyCount(value) {
  if (typeof value !== 'number'
    && (typeof value !== 'string' || !/^\d+(?:\.\d+)?$/u.test(value.trim()))) return null;
  const count = Number(value);
  return Number.isFinite(count) && count >= 1 && count <= 100_000_000 ? Math.round(count) : null;
}

export function verifiedReach(data, media) {
  if (!Array.isArray(data)) return null;
  const unit = media === 'bus' ? '人次/辆/天' : '人次/点位/天';
  const accepted = data.filter((entry) => entry?.type === 'coverPeoplesPVAccurate'
    && typeof entry.unit === 'string' && entry.unit.normalize('NFKC').trim() === unit);
  return accepted.length === 1 ? dailyCount(accepted[0].time1) : null;
}

export function cloudClient(cookie) {
  if (typeof cookie !== 'string' || !cookie || /[\r\n]/u.test(cookie)) throw new Error('Missing Newcloud session');
  const read = (path, options = {}) => request(cloudOrigin, path, {
    headers: { Cookie: cookie, 'X-Requested-With': 'XMLHttpRequest' }, referer: cloudPath('COVERAGE_PAGE'), ...options,
  });
  return {
    async coverage(names, radius, media, hasSupplemental) {
      const centers = [];
      for (const name of names) {
        if (contestCenters.has(name)) {
          const [lng, lat] = contestCenters.get(name);
          centers.push({ name, address: name, lng, lat });
          continue;
        }
        const found = await read(cloudPath('GEOCODE'), { json: { address: name, city: '上海市' } });
        if (!coordinate(found?.lng, found?.lat) || (Number(found?.confidence) > 0 && Number(found.confidence) < 50)) {
          throw new Error('Newcloud did not verify a requested landmark: ' + name);
        }
        centers.push({ name, address: found.address || name, lng: Number(found.lng), lat: Number(found.lat) });
      }
      const mediaTypes = ['bus', 'metro'].filter((kind) => media === 'both' || media === kind || (kind === 'bus' && hasSupplemental));
      const rows = await read(cloudPath('COVERAGE'), {
        json: { radius, mediaTypes, lineScope: 'self', centers },
      });
      if (!Array.isArray(rows)) throw new Error('Newcloud did not provide coverage rows');
      return { rows, csv: coverageCsv(rows, centers, radius) };
    },
    async routeCatalog() {
      const result = await read(cloudPath('ROUTES'), { referer: cloudPath('RENDERER') });
      if (!Array.isArray(result)) throw new Error('Newcloud did not provide the official line catalog');
      const routes = new Map();
      function collect(nodes) {
        for (const item of nodes) {
          if (typeof item.lineName === 'string' && typeof item.linePath === 'string' && item.linePath) {
            const key = routeKey(item.lineName);
            if (!routes.has(key)) routes.set(key, item.linePath);
          }
          if (Array.isArray(item.children)) collect(item.children);
        }
      }
      collect(result);
      return routes;
    },
    async route(linePath) {
      const result = await read(cloudPath('GEOMETRY'), { query: { linePath }, referer: cloudPath('RENDERER') });
      return normalizePath(result);
    },
    async labels(media) {
      const readLabels = async (firstLabel) => {
        const rows = await read(cloudPath('LABELS'), {
          form: { firstLabel, secondLabel: '' }, referer: cloudPath('LABEL_PAGE'),
        });
        if (!Array.isArray(rows)) throw new Error('Newcloud did not provide media labels');
        return rows;
      };
      if (media === 'bus') return readLabels('公交');
      const groups = await read(cloudPath('LABEL_GROUPS'), { form: {}, referer: cloudPath('LABEL_PAGE') });
      if (!Array.isArray(groups)) throw new Error('Newcloud did not provide media categories');
      const metroGroups = Array.from(new Set(groups.map((group) => group.firstLabel)
        .filter((name) => typeof name === 'string' && /^地铁(?:站|站点|出入口|出口|口)?$/u.test(name))));
      const results = await Promise.all(metroGroups.map(readLabels));
      return Array.from(new Map(results.flat().map((row) => [row.id, row])).values());
    },
    async reach(labelId, month) {
      if (!/^[\w-]{1,80}$/u.test(String(labelId))) throw new Error('Invalid Newcloud label');
      if (!/^2023-(0[1-9]|1[0-2])$/u.test(month)) throw new Error('Invalid Newcloud observation month');
      const dataMonth = month.replace('-', '');
      const result = await read(cloudPath('REACH'), { referer: cloudPath('REACH_PAGE'), form: {
        queryType: '1', indicatorType: 'visitorsFlowrateAndcrowdSpeed', dataMonth,
        labelIds: labelId, gridIds: '', extOptions: JSON.stringify({ gridType: '500', underground: '0' }),
      } });
      if (Array.isArray(result) && result.some(row => row?.type === 'coverPeoplesPVAccurate'
        && row.dataMonth != null && ![month, dataMonth].includes(String(row.dataMonth)))) {
        throw new Error('Newcloud returned an unexpected observation month');
      }
      return result;
    },
  };
}

const inventoryPageSize = 20;
const inventoryMaxRows = 3999;

function inventoryCount(value) {
  if (typeof value !== 'number' && (typeof value !== 'string' || !/^\d+$/u.test(value))) return null;
  const count = Number(value);
  return Number.isSafeInteger(count) && count >= 0 ? count : null;
}

function inventoryText(value, optional = false) {
  if (value == null && optional) return '';
  if (typeof value !== 'string' && !(typeof value === 'number' && Number.isSafeInteger(value))) {
    throw new Error('Redbean returned an invalid inventory field');
  }
  const text = String(value).trim();
  if ((!optional && !text) || text.length > 160 || /[\p{Cc}]/u.test(text)) {
    throw new Error('Redbean returned an invalid inventory field');
  }
  return text;
}

function inventoryRow(row, media) {
  const free = inventoryCount(row?.freeCount);
  if (free === null || free > 4_294_967_295) throw new Error('Redbean did not return a valid freeCount');
  const name = inventoryText(row.lineName);
  const grade = inventoryText(row.lineLevel, true);
  return media === 'bus'
    ? [name, inventoryText(row.companyName), grade, inventoryText(row.mediaName), free]
    : [inventoryText(row.metroNumber), name, inventoryText(row.exitName), grade, free];
}

function inventoryCsv(rows, media) {
  const heading = media === 'bus'
    ? ['线路名称', '行车公司', '线路等级', '媒体名称', '空车数']
    : ['地铁编号', '站点名称', '出入口号', '媒体等级', '空位数'];
  return Buffer.from('\ufeff' + [heading, ...rows].map(csvRow).join('\r\n') + '\r\n', 'utf8');
}

export function redClient(token, tenant = '1') {
  if (!/^[A-Za-z0-9._~-]{6,4096}$/u.test(token) || !/^\d{1,12}$/u.test(tenant)) throw new Error('Missing Redbean session');
  const read = async (key, query, excel = false) => {
    const path = redPath('API_PREFIX') + redPath(key);
    const options = {
      headers: { Authorization: 'Bearer ' + token, 'tenant-id': tenant },
      referer: redPath('LOGIN'), query: { pageNo: 1, pageSize: 10, ...query }, excel,
    };
    try {
      return await request(redOrigin, path, options);
    } catch (error) {
      const temporary = error instanceof TypeError || error.name === 'TimeoutError'
        || [408, 500, 502, 503, 504].includes(error.status);
      if (!temporary) throw error;
      await new Promise(resolve => setTimeout(resolve, 350));
      return request(redOrigin, path, options);
    }
  };
  let levelDictionary;
  async function loadLevels() {
    let entries;
    try {
      entries = await read('DICTIONARY');
    } catch (error) {
      if (sessionRejected(error)) throw error;
      throw new Error('Redbean media-level dictionary is unavailable');
    }
    if (!Array.isArray(entries)) throw new Error('Redbean returned an incompatible media-level dictionary');
    const labels = new Map();
    for (const entry of entries) {
      if (entry?.dictType !== 'medium_level_type') continue;
      const code = inventoryText(entry.value);
      const label = inventoryText(entry.label);
      if (labels.has(code) && labels.get(code) !== label) throw new Error('Redbean media-level dictionary has conflicts');
      labels.set(code, label);
    }
    return labels;
  }
  async function resolveLevels(rows, media) {
    const index = media === 'bus' ? 2 : 3;
    const coded = rows.filter((row) => row[index] && !/^[A-Z][+-]{0,3}$/iu.test(row[index]));
    if (!coded.length) return;
    const labels = await (levelDictionary ??= loadLevels());
    for (const row of coded) {
      const label = labels.get(row[index]);
      if (!label) throw new Error('Redbean returned an unknown media level');
      row[index] = label;
    }
  }
  async function page(media, dates, filter, pageNo, pageSize) {
    const response = await read(media === 'bus' ? 'BUS_PAGE' : 'METRO_PAGE', {
      ...dates, ...filter, pageNo, pageSize,
    });
    const total = inventoryCount(response?.total);
    const start = (pageNo - 1) * pageSize;
    if (total === null || total > inventoryMaxRows || !Array.isArray(response?.list)
      || response.list.length !== Math.min(pageSize, Math.max(total - start, 0))) {
      throw new Error('Redbean returned an incomplete or incompatible inventory page');
    }
    return { total, list: response.list };
  }
  async function inventory(media, dates, filter = {}) {
    const first = await page(media, dates, filter, 1, inventoryPageSize);
    const pages = Math.ceil(first.total / inventoryPageSize);
    const rows = first.list.map((row) => inventoryRow(row, media));
    for (let start = 2; start <= pages; start += 3) {
      const numbers = Array.from({ length: Math.min(3, pages - start + 1) }, (_, index) => start + index);
      const next = await Promise.all(numbers.map((number) => page(media, dates, filter, number, inventoryPageSize)));
      for (const result of next) {
        if (result.total !== first.total) throw new Error('Redbean inventory changed while reading its pages');
        rows.push(...result.list.map((row) => inventoryRow(row, media)));
      }
    }
    const ids = rows.map((row) => media === 'metro' ? row[0] : JSON.stringify(row.slice(0, -1)));
    if (new Set(ids).size !== ids.length) throw new Error('Redbean inventory contains duplicate page rows');
    return rows;
  }
  async function stockRows(media, dates, filter = {}, requiredRoutes = []) {
    const rows = await inventory(media, dates, filter);
    const found = new Set(rows.map(([name]) => routeKey(name)));
    for (const route of requiredRoutes) {
      const key = routeKey(route);
      if (found.has(key)) continue;
      const scoped = await inventory('bus', dates, { lineName: key });
      rows.push(...scoped.filter(([name]) => routeKey(name) === key));
      found.add(key);
    }
    await resolveLevels(rows, media);
    return rows;
  }
  return {
    authorize: (dates, media) => Promise.all((media === 'both' ? ['bus', 'metro'] : [media])
      .map((kind) => page(kind, dates, {}, 1, 1))),
    stockRows,
    bus: async (dates, requiredRoutes = []) =>
      inventoryCsv(await stockRows('bus', dates, {}, requiredRoutes), 'bus'),
    metro: async (dates, keyword) =>
      inventoryCsv(await stockRows('metro', dates, keyword ? { keyword } : {}), 'metro'),
    busExport: (dates) => read('BUS_EXPORT', dates, true),
    metroExport: (dates) => read('METRO_EXPORT', dates, true),
    specs: () => read('SPECS', {}, true),
  };
}

export async function archive(directory, file, bytes) {
  await writeFile(join(directory, file), bytes);
}
