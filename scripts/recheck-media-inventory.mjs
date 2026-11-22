import { redClient, routeKey, sessionRejected } from './contest-http.mjs';

const key = value => value.normalize('NFKC').toLowerCase().replace(/[\s()]/gu, '');
const busKey = value => routeKey(key(value));
const metroName = value => key(value).replace(/号(?:出入口|出口)/gu, '号口');

function matches(row, demand, medium) {
  if (medium === 'bus') {
    const [route, operator, grade, format] = row;
    return busKey(route) === busKey(demand.name) && key(operator) === key(demand.operator)
      && key(grade) === key(demand.grade) && key(format) === key(demand.format);
  }
  const [code, station, exit, grade] = row;
  const name = station + (/^[A-Za-z0-9]+$/u.test(exit) ? exit + '号口' : exit);
  return (!demand.code || key(code) === key(demand.code))
    && metroName(name) === metroName(demand.name) && key(grade) === key(demand.grade);
}

function findIssues(rows, demands, medium, period) {
  const issues = [];
  const usage = new Map();
  for (const { index, demand } of demands) {
    if (!Number.isSafeInteger(demand.quantity) || demand.quantity < 1) {
      throw new Error('Invalid inventory demand');
    }
    const matchesDemand = rows.filter(row => matches(row, demand, medium));
    if (matchesDemand.length !== 1) {
      issues.push({
        period, demands: [index], kind: matchesDemand.length ? 'ambiguous' : 'missing_or_changed',
      });
      continue;
    }
    const [row] = matchesDemand;
    const allocation = usage.get(row) ?? { total: 0, indexes: [] };
    allocation.total += demand.quantity;
    allocation.indexes.push(index);
    usage.set(row, allocation);
  }
  for (const [row, { total, indexes }] of usage) {
    const available = row.at(-1);
    if (total > available) {
      issues.push({ period, demands: indexes, kind: 'insufficient', available });
    }
  }
  return issues;
}

async function main() {
  const input = [];
  let size = 0;
  for await (const chunk of process.stdin) {
    size += chunk.length;
    if (size > 8 * 1024 * 1024) return 4;
    input.push(chunk);
  }
  const periods = JSON.parse(Buffer.concat(input).toString('utf8'));
  if (!Array.isArray(periods) || !periods.length || periods.length > 12) return 4;
  const client = redClient(process.env.CITYLINE_REDBEAN_ACCESS_TOKEN, process.env.CITYLINE_REDBEAN_TENANT_ID || '1');
  const issues = [];
  for (const [period, { starts, ends, demands }] of periods.entries()) {
    if (!demands.length || demands.some(item => !['bus', 'metro'].includes(item.medium))) return 4;
    const dates = {startDate: starts, endDate: ends};
    const indexed = demands.map((demand, index) => ({ index, demand }));
    const result = await Promise.all(['bus', 'metro'].map(async medium => {
      const selected = indexed.filter(({ demand }) => demand.medium === medium);
      if (!selected.length) return [];
      const required = medium === 'bus' ? selected.map(({ demand }) => demand.name) : [];
      const rows = await client.stockRows(medium, dates, {}, required);
      return findIssues(rows, selected, medium, period);
    }));
    issues.push(...result.flat());
  }
  if (issues.length) {
    process.stdout.write(JSON.stringify(issues));
    return 2;
  }
  return 0;
}

try {
  process.exitCode = await main();
} catch (error) {
  process.exitCode = sessionRejected(error) ? 3 : 4;
}
