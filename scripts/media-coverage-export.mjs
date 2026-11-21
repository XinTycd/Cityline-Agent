import { parse } from 'csv-parse/sync';

function canonical(value) {
  return (value ?? '').normalize('NFKC').replace(/\s/gu, '').toLowerCase();
}

function radiusMeters(value) {
  const match = /^([0-9]+(?:\.[0-9]+)?)(米|公里)$/u.exec(canonical(value));
  return match ? Math.round(Number(match[1]) * (match[2] === '公里' ? 1000 : 1)) : undefined;
}

export function coverageCandidates(csv, { center, supplementalCenter = '', radius, media }) {
  const rows = parse(csv, {
    bom: true,
    relax_column_count: true,
    relax_quotes: true,
    skip_empty_lines: true,
    record_delimiter: ['\r\n', '\n', '\r'],
  });
  const marker = rows.findIndex((row) => canonical(row[0]) === '明细分析');
  const header = rows[marker + 1]?.map(canonical);
  if (marker < 0 || !header) throw new Error('Missing coverage details');
  const [point, radiusColumn, type, name, station] = ['中心点名称', '覆盖半径', '媒体类型', '媒体名称', '最近站点']
    .map((key) => header.indexOf(key));
  if ([point, radiusColumn, type, name].some((index) => index < 0)) throw new Error('Unexpected coverage columns');

  const requestedCenters = new Set(center.split('|').map(canonical));
  const supplemental = canonical(supplementalCenter);
  const lines = new Set();
  const stations = new Set();
  for (const row of rows.slice(marker + 2)) {
    const sourceCenter = canonical(row[point]);
    const primary = requestedCenters.has(sourceCenter);
    if (!primary && (!supplemental || sourceCenter !== supplemental)) continue;
    const bus = ['公交线路', '公交车身'].includes(canonical(row[type]));
    const metro = ['地铁出入口', '地铁出口'].includes(canonical(row[type]));
    if ((!bus || !primary || media === 'metro') && (!metro || media === 'bus')) continue;
    if (radiusMeters(row[radiusColumn]) !== radius) throw new Error('Coverage radius does not match request');

    if (bus) {
      const route = /^(\d+)路?$/u.exec(canonical(row[name]))?.[1];
      lines.add(route ?? '');
    }
    if (metro) {
      const value = row[station]?.normalize('NFKC').trim();
      if (!value) throw new Error('Missing subway station in official coverage');
      stations.add(value);
    }
  }
  if ((media !== 'metro' && lines.size === 0) || (media !== 'bus' && stations.size === 0)) {
    throw new Error('Selected coverage is unavailable');
  }
  return { lines, station: stations.size === 1 ? [...stations][0] : null };
}
