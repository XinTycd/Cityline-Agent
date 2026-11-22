import assert from 'node:assert/strict';
import { mkdtemp, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

const workbench = 'http://127.0.0.1:3512';
const newcloud = 'https://newcloud.example.invalid';
Object.assign(process.env, {
  CITYLINE_NEWCLOUD_ORIGIN: newcloud,
  CITYLINE_REDBEAN_ORIGIN: '',
  CITYLINE_NEWCLOUD_CENTER_COORDINATES: '[]',
  CITYLINE_NEWCLOUD_COVERAGE_PAGE: '/coverage',
  CITYLINE_NEWCLOUD_LABELS: '/labels',
  CITYLINE_NEWCLOUD_LABEL_GROUPS: '/groups',
  CITYLINE_NEWCLOUD_LABEL_PAGE: '/media',
  CITYLINE_NEWCLOUD_REACH: '/reach',
  CITYLINE_NEWCLOUD_REACH_PAGE: '/media',
});
const { cloudClient } = await import('./contest-http.mjs');
const { acquireDirectEvidence } = await import('./contest-direct-evidence.mjs');

const bus = { media: 'bus', id: '10路', name: '10路', exact_reach: [], baseline_reach: [] };
const exit = { media: 'metro', id: 'A01', name: '测试站1号口', exact_reach: [], baseline_reach: [] };
const otherExit = { ...exit, id: 'B01', name: '演练站2号口', station: '演练站' };
const extraBuses = ['11路', '12路', '13路', '14路'].map(name => ({ ...bus, name, id: name }));
const period = (starts, ends, months, assets) => ({
  starts, ends, months, assets, batch: starts, geometry_routes: [],
});
const periods = [
  period('2027-07-15', '2027-08-14', ['2023-07', '2023-08'], [bus, exit, otherExit]),
  period('2027-08-18', '2027-08-22', ['2023-08'], [exit]),
  period('2027-10-01', '2027-10-31', ['2023-10'], [bus]),
];
const saved = [];
const requests = [];
const observation = (month, count, unit) => ({
  type: 'coverPeoplesPVAccurate', dataMonth: month, time1: count, unit,
});
globalThis.fetch = async (input, options) => {
  const url = new URL(input);
  if (url.origin === workbench) {
    if (options.method !== 'POST') return Response.json({ periods, overview_routes: [] });
    saved.push(await options.body.get('reach').text());
    return new Response(null, { status: 204 });
  }
  assert.equal(url.origin, newcloud);
  if (url.pathname === '/groups') return Response.json([{ firstLabel: '地铁出入口' }]);
  if (url.pathname === '/labels') {
    const busLabels = [{ id: 'bus', thirdLabel: '10路' },
      ...extraBuses.map((asset, index) => ({ id: `bus${index + 1}`, thirdLabel: asset.name }))];
    return Response.json(options.body.get('firstLabel') === '公交' ? busLabels : [
      { id: 'exit', thirdLabel: '测试站1号口' }, { id: 'station', thirdLabel: '演练站' },
    ]);
  }
  assert.equal(url.pathname, '/reach');
  const id = options.body.get('labelIds');
  const month = options.body.get('dataMonth');
  requests.push(`${id}/${month}`);
  if (id === 'bus' && month === '202308') return Response.json([]);
  if (id === 'bus' && month === '202310') return Response.json([observation('202305', 99, '人次/辆/天')]);
  if (/^bus[123]$/u.test(id) && month === '202309') return Response.json({ code: 422 });
  return Response.json([observation(month, 3000, id.startsWith('bus') ? '人次/辆/天' : '人次/点位/天')]);
};

const directory = await mkdtemp(join(tmpdir(), 'verify-newcloud-reach-'));
try {
  const run = () => acquireDirectEvidence(cloudClient('test-session'), directory, workbench, {}, async () => {});
  process.env.CITYLINE_NEWCLOUD_REACH_MONTHS = '';
  const { overview, reachChecks } = await run();
  assert.deepEqual(requests, [
    'bus/202307', 'bus/202308', 'bus/202305', 'exit/202307', 'exit/202308',
    'station/202307', 'station/202308', 'station/202305', 'bus/202310',
  ]);
  assert.equal(overview.expectedReach, 8);
  assert.equal(overview.reach, 6);
  assert.equal(overview.baselineReach, 2);
  assert.deepEqual(reachChecks.map(check => [check.resource, check.month, check.exact]), [
    ['B01', 7, 'station-only'], ['B01', 8, 'station-only'],
  ]);
  assert.match(saved[0], /"公交车身","10路","2023-07","3000","人次\/辆\/天"/u);
  assert.match(saved[0], /"地铁出入口","B01","2023-08","3000","平台标签日均人次"/u);
  assert.doesNotMatch(saved.join('\n'), /"99"/u);
  requests.length = 0;
  process.env.CITYLINE_NEWCLOUD_REACH_MONTHS = '2023-05';
  assert.equal((await run()).overview.baselineReach, 6);
  assert.deepEqual(requests, ['bus/202305', 'exit/202305', 'station/202305']);
  requests.length = 0;
  process.env.CITYLINE_NEWCLOUD_REACH_MONTHS = '';
  periods.splice(0, periods.length, period('2027-09-01', '2027-09-30', ['2023-09'], extraBuses));
  const afterErrors = (await run()).overview;
  assert.equal(afterErrors.reach, 4);
  assert.equal(afterErrors.baselineReach, 3);
  assert.ok(requests.includes('bus4/202309'));
} finally {
  await rm(directory, { recursive: true, force: true });
}
console.log('新云逐档期获取数据验证通过。已覆盖同月优先、跨段复用、历史基准、错月响应和站级数据。');
