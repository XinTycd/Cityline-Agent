import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { readFile } from 'node:fs/promises';

const port = Number(process.env.ACCEPTANCE_RUST_PORT ?? 3412);
assert.ok(Number.isInteger(port) && port >= 1 && port <= 65535);
const origin = `http://127.0.0.1:${port}`;
const root = '/media-proposals';

async function page(path, expected = 200) {
  const response = await fetch(origin + path, { signal: AbortSignal.timeout(10000) });
  assert.equal(response.status, expected, path);
  return response;
}

const home = await (await page('/')).text();
assert.match(home, /<title>城线 · 提案任务<\/title>/u);
assert.match(home, /一键生成提案/u);
assert.match(home, /从 4A 客户 Brief 提取需求/u);
assert.match(home, /食品饮料行业 · 上海商圈到店提案/u);
assert.match(home, /180–250 万元/u);
assert.match(home, /data-fill-campaign-case/u);
assert.doesNotMatch(home, /name="bus_stock"|费用区间/u);
assert.doesNotMatch(home, /name="source"/u);
assert.equal((await page(root)).headers.get('referrer-policy'), 'same-origin');
const data = await (await page('/media-data')).text();
assert.match(data, /平台快照/u);
assert.match(data, /发起提案并获取数据/u);
assert.doesNotMatch(data, /手动补录赛用数据/u);
const localCatalog = await (await page('/media-data?source=local')).text();
assert.match(localCatalog, /手动补录赛用数据/u);
assert.match(localCatalog, /本地快照/u);
assert.match(localCatalog, /action="\/api\/media-proposals\/import\?view=data"/u);
const connections = await (await page('/connections')).text();
assert.match(connections, /<h2>新云平台<\/h2>/u);
assert.match(connections, /<h2>红豆平台<\/h2>/u);
const demoPath = '?demo=1&budget=200&compare=100&media=both';
const demo = await (await page(root + demoPath)).text();
assert.match(demo, /与 100 万预算方案比较/u);
assert.match(demo, /所选档期无可售余量/u);
assert.match(demo, /保留 \d+ 项 · 新增 [1-9]\d* 项/u);
const exportDemo = await page(root + '/export' + demoPath);
assert.match(exportDemo.headers.get('content-disposition'), /cityline-food-both-200w-2027-06-01\.html/u);
const offline = await exportDemo.text();
assert.doesNotMatch(offline, /本案的资源和数量与/u);
const workflowCopy = /data-review-status|data-approved-variant|未审批|待审批|审批中|待审核|人工确认采用|确认时间|备选草案 · 待(?:客户)?确认/u;
assert.doesNotMatch(offline, workflowCopy);
function verifyCampaignRequirements(html) {
  const context = html.match(/aria-label="品牌与目标">([\s\S]*?)<\/dl>/u)?.[1] ?? '';
  assert.deepEqual([...context.matchAll(/<dt>([^<]+)<\/dt>/gu)].map((match) => match[1]), [
    '客户与品牌', '生意目标', '投放主目标', '目标人群与活动区域',
  ]);
  assert.match(context, /食品饮料/u);
  assert.match(html, /<dt>投放档期<\/dt>[\s\S]*<dt>媒体范围<\/dt>|<dt>媒体范围<\/dt>[\s\S]*<dt>投放档期<\/dt>/u);
  assert.match(html, /<dt>(客户预算量级|整案预算量级)<\/dt>/u);
  assert.match(html, /id="measurement">效果衡量与投放验收[\s\S]*投放证明[\s\S]*带日期的上下刊照片/u);
  assert.doesNotMatch(html, /Campaign 假设书|aria-label="投放需求七项"/u);
  assert.ok(html.indexOf('aria-label="品牌与目标"') < html.indexOf('id="proposal-variants"'));
  assert.ok(html.indexOf('id="measurement"') > html.indexOf('id="proposal-variants"'));
}
verifyCampaignRequirements(offline);
assert.match(offline, /12 家示例门店/u);
assert.match(offline, /KPI 与统计口径[\s\S]*有效到店试饮扫码量/u);
assert.match(offline, /<details class="media-item">/u);
assert.match(offline, /<body class="report-page">/u);
assert.match(offline, /class="report-schedule"/u);
for (const heading of ['公司简介', '目标圈与媒介组合', '发布推荐', '采购清单与刊例', '增值权益与交付']) {
  assert.ok(offline.includes(heading), heading);
}
assert.equal((offline.match(/客户预算量级<\/dt><dd>约 200 万元<\/dd>/gu) ?? []).length, 1);
const generatedAt = /<time datetime="(\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}\+08:00)">/u;
assert.match(offline, generatedAt);
assert.match(offline, /报价范围为采购表所列项目/u);
const reportScripts = [...offline.matchAll(/<script>([\s\S]*?)<\/script>/gu)];
assert.equal(reportScripts.length, 1);
const reportScriptPolicy = `script-src 'sha256-${createHash('sha256').update(reportScripts[0][1]).digest('base64')}'`;
assert.ok(offline.includes(`content="${reportScriptPolicy}; default-src 'none'`));
assert.doesNotMatch(offline, /<script\s|<link\b/u);
const css = await (await page('/assets/workbench.css')).text();
assert.ok(offline.includes('<style>' + css + '</style>'), '报告与工作台应使用同一份样式');
const preview = await page(root + '/export' + demoPath + '&preview=1&theme=dark');
assert.equal(preview.headers.get('content-disposition'), null);
assert.match(preview.headers.get('content-security-policy'), /default-src 'none'; style-src 'unsafe-inline'/u);
assert.ok(preview.headers.get('content-security-policy').includes(reportScriptPolicy));
assert.match(await preview.text(), /<html lang="zh-CN" data-theme="light">/u);
for (const path of ['/projects', '/assignments', '/verification', '/relay', '/self-check', '/api/samples/ready']) {
  await page(path, 404);
}
for (const path of ['/overview', '/task-log', '/agents', '/audit', '/settings']) await page(path);

const profile = await (await page('/profile')).text();
assert.match(profile, /当前登录账号/u);
assert.match(profile, /href="\/profile" aria-label="个性化设置"/u);
assert.match(profile, /<strong>演示账号<\/strong><p>本机演示会话/u);
assert.match(profile, /value="system" checked>跟随系统/u);
assert.match(profile, /aria-label="退出登录"/u);
const sessionPost = (path, requestOrigin) => fetch(origin + path, {
  method: 'POST', headers: { Origin: requestOrigin }, redirect: 'manual',
  signal: AbortSignal.timeout(10000),
});
assert.equal((await sessionPost('/demo/sign-out', 'https://outside.example')).status, 403);
const signedOut = await sessionPost('/demo/sign-out', origin);
assert.equal(signedOut.status, 303);
assert.equal(signedOut.headers.get('location'), '/demo/sign-in');
const cookie = signedOut.headers.get('set-cookie');
assert.match(cookie ?? '', /hackson_demo_signed_out=1; Path=\/; HttpOnly; SameSite=Strict/u);
const gated = await fetch(origin + root, {
  headers: { Cookie: cookie.split(';')[0] }, redirect: 'manual',
});
assert.equal(gated.status, 303);
assert.equal(gated.headers.get('location'), '/demo/sign-in');
const gatedQueues = await fetch(origin + '/overview/queues', {
  headers: { Cookie: cookie.split(';')[0] }, redirect: 'manual',
});
assert.equal(gatedQueues.status, 303);
assert.equal(gatedQueues.headers.get('location'), '/demo/sign-in');
assert.match(await (await page('/demo/sign-in')).text(), /进入工作台/u);
const restored = await sessionPost('/demo/sign-in', origin);
assert.equal(restored.status, 303);
assert.equal(restored.headers.get('location'), root);
assert.match(restored.headers.get('set-cookie') ?? '', /Max-Age=0/u);
for (const industry of ['bank', 'health', 'hospital', 'park']) {
  await page(root + '?demo=1&industry=' + industry, 422);
  await page(root + '/export?demo=1&industry=' + industry, 422);
}
for (const amount of [100, 200, 500]) {
  for (const medium of ['bus', 'metro', 'both']) {
    const scenario = '?demo=1&industry=food&budget=' + amount + '&media=' + medium;
    const html = await (await page(root + scenario)).text();
    assert.ok(html.includes('上海某连锁食品饮料品牌'), scenario);
    const total = html.match(/费用区间<\/dt><dd>¥(\d+)–(\d+)/u);
    assert.ok(total && Number(total[1]) > 0 && Number(total[2]) <= amount * 10_000, scenario);
    assert.match(html, /下载演示 HTML/u, scenario);
  }
}
await page(root + '?budget=300', 400);
await page(root + '/export?demo=1&starts=2027-07-01&ends=2027-07-31', 409);

const sourceDate = { starts: '2027-07-01', ends: '2027-07-31' };
function file(form, name, text) {
  form.set(name, new Blob([text], { type: 'text/csv' }), name + '.csv');
}

async function importData(fields, addEvidence = true, view = '') {
  const form = new FormData();
  for (const [name, value] of Object.entries(fields)) form.set(name, value);
  file(form, 'bus_stock', '线路名称,行车公司,线路等级,媒体名称,空车数\n10,测试运营方,A+,大三侧,2\n11,测试运营方,A+,大三侧,0\n');
  file(form, 'bus_coverage', '线路名称,目标圈,半径\n10,目标商圈,300\n11,备选商圈,500\n12,另一商圈,300\n');
  file(form, 'metro_stock', '点位名称,站点等级,媒体形式,可售数量,面积\n演示站A号口,A+,灯箱,1,9.1\n');
  file(form, 'metro_coverage', '点位名称,目标圈,半径\n演示站A号口,地铁目标圈,250\n');
  if (addEvidence) {
    // Isolated test data for the documented media units; never submit to the contest database.
    const months = ['07', '09', '10'];
    const lineData = months.flatMap((month) => [
      `公交车身,10路,2023-${month},1000,人次/辆/天,测试公交原行`,
      `地铁出入口,演示站A号口,2023-${month},600,人次/点位/天,测试地铁原行`,
    ]);
    file(form, 'reach', ['媒体类型,资源编号或线路,统计月份,每日触达人次,计量单位,来源记录', ...lineData].join('\n'));
    const mapData = {
      type: 'FeatureCollection', features: [
        { type: 'Feature', properties: { media: 'bus', resource: '10路', source_ref: '测试公交轨迹' },
          geometry: { type: 'LineString', coordinates: [[121.451, 31.221], [121.469, 31.237]] } },
        { type: 'Feature', properties: { media: 'metro', resource: '演示站A号口', source_ref: '测试地铁出口' },
          geometry: { type: 'Point', coordinates: [121.46, 31.226] } },
      ],
    };
    form.set('route_geometry', new Blob([JSON.stringify(mapData)], { type: 'application/geo+json' }), 'test-geometry.geojson');
  }
  const tariff = process.env.MEDIA_TARIFF_FILE;
  if (tariff) {
    form.set('rates', new Blob([await readFile(tariff)]), '07-媒体刊例价.xlsx');
  } else {
    file(form, 'rates', '媒体类型,匹配对象,发布费,计价数量,计价周期,制作费\n公交车,A+,约3–5万,1,1月,约5–10千\n地铁出入口,A+,约15–25万,1,1月,100元/㎡\n');
  }
  return fetch(origin + '/api/media-proposals/import' + (view ? '?view=' + view : ''), {
    method: 'POST',
    headers: { Origin: origin },
    body: form,
    redirect: 'manual',
    signal: AbortSignal.timeout(10000),
  });
}

const input = {
  industry: 'food',
  ...sourceDate,
  observed_at: '2026-11-21T10:00',
  radius: '300',
  circle: '目标商圈',
  attested: 'yes',
};
const crossOrigin = await fetch(origin + '/api/media-proposals/import', {
  method: 'POST', headers: { Origin: 'https://outside.example' }, signal: AbortSignal.timeout(10000),
});
assert.equal(crossOrigin.status, 403);
const pretendWorker = await fetch(origin + '/api/media-proposals/import', {
  method: 'POST', headers: { Origin: origin, 'X-Cityline-Media-Import': '00000000-0000-4000-8000-000000000001' },
});
assert.equal(pretendWorker.status, 403);
assert.equal((await importData({ ...input, ends: '2027-02-30' })).status, 422);
const received = await importData(input);
assert.equal(received.status, 303, await received.text());
const location = received.headers.get('location');
assert.ok(location?.startsWith(root + '?'));
const imported = await (await page(location)).text();
assert.match(imported, /已导入赛用数据/u);
assert.match(imported, /¥221000–371000/u);
assert.match(imported, /红豆库存依据 · bus_stock.csv 第 2 行/u);
assert.match(imported, /快照来源 · 人工导入/u);
assert.match(imported, /覆盖 地铁目标圈（250 米）/u);
assert.match(imported, /所选档期库存表缺少对应资源/u);
assert.match(imported, /所选档期无可售余量/u);
async function recoverImported(response, budget) {
  assert.equal(response.status, 303);
  const path = response.headers.get('location');
  const initial = await completedTask(path);
  assert.equal(initial.state, 'review');
  assert.equal(initial.login.phase, 'error_preflight');
  assert.equal(initial.platform_interrupted, true);
  assert.equal(initial.current_step, 1);
  assert.match(initial.steps[1].notes.join(' '), /本机尚未设置赛题刊例路径/u);
  assert.ok(initial.steps[2].notes.length > 0, '需求策略先于平台预检');
  assert.deepEqual(initial.steps[3].notes, [], '无库存时不启动媒介组合 Agent');
  const recovered = await fetch(origin + path + '/rerun', {
    method: 'POST', headers: { Origin: origin }, redirect: 'manual',
    body: new URLSearchParams({ budget, local_snapshot: '1' }),
  });
  if (recovered.status === 409) return response;
  if (recovered.status !== 303) assert.fail(await recovered.text());
  return recovered;
}

const run = async (options) => recoverImported(await fetch(origin + root + '/run', {
  method: 'POST', headers: { Origin: origin }, redirect: 'manual',
  body: new URLSearchParams(options), signal: AbortSignal.timeout(10000),
}), options.budget);
const executed = await run({industry: 'food', month: '2027-07', media: 'both', budget: '200'});
assert.equal(executed.status, 303);
const emptyGoalPath = executed.headers.get('location');
assert.match(emptyGoalPath ?? '', /^\/media-proposals\/tasks\/[a-f0-9-]{36}$/u);
async function completedTask(path) {
  const deadline = Date.now() + 125000;
  while (Date.now() < deadline) {
    const status = await (await page('/api' + path)).json();
    if (status.state !== 'active') return status;
    await new Promise((resolve) => setTimeout(resolve, 350));
  }
  assert.fail('提案任务未在时限内结束');
}
async function confirmTaskGoal(path, goal = '认知') {
  await completedTask(path);
  const confirmed = await fetch(origin + path + '/confirm-goal', {
    method: 'POST', headers: { Origin: origin }, redirect: 'manual',
    body: new URLSearchParams({ goal }),
  });
  assert.equal(confirmed.status, 303, await confirmed.text());
  return confirmed.headers.get('location');
}
const emptyGoal = await completedTask(emptyGoalPath);
assert.equal(emptyGoal.state, 'review');
assert.equal(emptyGoal.offer_review, false);
assert.equal(emptyGoal.offer_goal_confirmation, true);
const emptyGoalApproval = await fetch(origin + emptyGoalPath + '/review', {
  method: 'POST', headers: { Origin: origin }, redirect: 'manual',
  body: new URLSearchParams({ decision: 'approved', variant: 'A', note: '测试空目标审批', accept_budget_gap: '1' }),
});
assert.equal(emptyGoalApproval.status, 409);
const taskPath = await confirmTaskGoal(emptyGoalPath);
const done = await completedTask(taskPath);
assert.equal(done.state, 'done');
assert.equal(done.has_agent_trace, true);
assert.equal(done.steps.length, 5);
assert.match(done.steps[1].notes.join(' '), /已锁定赛用数据快照/u);
assert.match(done.steps[4].notes.join(' '), /HTML 已渲染并保存/u);
const taskResult = await (await page(taskPath)).text();
assert.match(taskResult, /正式报告将在平台数据齐备并通过人工确认后开放/u);
assert.match(taskResult, /查看 Agent 协作/u);
const ruleTrace = await (await page(taskPath + '/agent-trace')).text();
assert.equal([...ruleTrace.matchAll(/>规则兜底<\/span>/gu)].length, 3);
assert.match(ruleTrace, /事实和证据缺项/u);
assert.doesNotMatch(ruleTrace, /input_summary|prompt_sha256|model_output/u);
assert.doesNotMatch(taskResult, /type="file"/u);
const taskReport = await page(taskPath + '/review-copy?theme=dark');
assert.equal(taskReport.headers.get('content-disposition'), null);
await page(taskPath + '/report?preview=1', 409);
await page(taskPath + '/report?delivery=1', 409);
const publishChecklist = await (await page(taskPath + '/evidence?scope=publication')).text();
assert.match(publishChecklist, /正式报告核验清单/u);
assert.match(publishChecklist, /当前库存来自本地或历史快照/u);
assert.match(publishChecklist, /当前坐标来自本地补录/u);
assert.match(publishChecklist, /部分月份采用本地触达记录/u);
assert.match(publishChecklist, /沿用本任务条件重新获取数据/u);
assert.doesNotMatch(publishChecklist, /单位日触达补录模板|地铁出口坐标补录模板/u);
const publishCsv = await (await page(taskPath + '/evidence?scope=publication&kind=reach&download=1')).text();
assert.match(publishCsv, /缺少平台确认的 人次\/辆\/天/u);
assert.doesNotMatch(publishCsv, /库存与组合|地铁出口实点/u);
await page(taskPath + '/evidence?scope=unknown', 400);
const taskHtml = await taskReport.text();
assert.match(taskHtml, /data-theme="light"/u);
assert.doesNotMatch(taskHtml, workflowCopy);
assert.match(taskHtml, /内部核验副本/u);
assert.equal((taskHtml.match(/name="report-view"/gu) ?? []).length, 3);
assert.match(taskHtml, /投放建议/u);
assert.match(taskHtml, /查看核验依据/u);
assert.match(taskHtml, generatedAt);
assert.equal((await (await page(taskPath + '/review-copy?preview=1')).text()).match(generatedAt)?.[1], taskHtml.match(generatedAt)?.[1]);
assert.match(taskHtml, /¥221000–371000/u);
assert.match(taskHtml, /最低高位报价合计 <strong>31\.10 万元<\/strong>/u);
assert.match(taskHtml, /地铁出入口 · 演示站A号口 · 灯箱 · ¥251000/u);
assert.equal((taskHtml.match(/<article class="report-variant"/gu) ?? []).length, 2);
assert.equal((taskHtml.match(/<polyline class="route-map-path"/gu) ?? []).length, 2);
assert.match(taskHtml, /日均 2,600 人次/u);
assert.match(taskHtml, /80,600 人次/u);
assert.doesNotMatch(taskHtml, /核心圈公交车日 · 距离待核/u);
assert.equal(done.offer_review, true);
assert.deepEqual(done.approval_readiness, { a: true, b: true });
const overviewQueues = async () => (await page('/overview/queues')).text();
const queueSection = (html, id) => html.split(`aria-labelledby="${id}"`)[1].split('</section>')[0];
const readyOverview = await overviewQueues();
assert.match(readyOverview, /data-overview-count="snapshots">[1-9][0-9]*<\/dd>/u);
assert.ok(queueSection(readyOverview,'ready-tasks').includes(`href="${taskPath}"`));
const demoTaskResult = await fetch(origin + root + '/run', {
  method: 'POST', headers: { Origin: origin }, redirect: 'manual',
  body: new URLSearchParams({ demo: '1', industry: 'food', budget: '200', media: 'both',
    starts: '2027-01-01', ends: '2027-12-31' }),
});
assert.equal(demoTaskResult.status, 303);
const demoTaskPath = demoTaskResult.headers.get('location');
const demoTask = await completedTask(demoTaskPath);
assert.equal(demoTask.state, 'demo');
assert.equal(demoTask.offer_review, false);
assert.equal(demoTask.approval_readiness, null);
assert.doesNotMatch(await (await page(demoTaskPath)).text(), /name="decision"/u);
const demoTaskReport = await (await page(demoTaskPath + '/report?preview=1')).text();
assert.match(demoTaskReport, /data-demo-report/u);
assert.match(demoTaskReport, /2027-01-01 至 2027-12-31/u);
assert.match(demoTaskReport, /店均日有效扫码量较基线提升至少 15%/u);
assert.doesNotMatch(demoTaskReport, workflowCopy);
const demoCalendar = demoTaskReport.split('id="calendar"')[1].split('</section>')[0];
for (const [month, amount] of [['05', 40], ['07', 120], ['10', 40]]) {
  assert.match(demoCalendar, new RegExp(
    '2027-' + month + '-01 至 2027-' + month + '-31</strong>[\\s\\S]*?计划预算 ' + amount + '\\.00 万元', 'u',
  ));
}
await page(demoTaskPath + '/report?delivery=1', 409);
for (const decision of ['approved', 'returned']) {
  const rejected = await fetch(origin + demoTaskPath + '/review', {
    method: 'POST', headers: { Origin: origin }, redirect: 'manual',
    body: new URLSearchParams({ decision, variant: 'A', note: '隔离验收' }),
  });
  assert.equal(rejected.status, 409);
}
assert.ok(!(await (await page('/overview')).text()).includes(demoTaskPath));
assert.ok(!(await overviewQueues()).includes(demoTaskPath));
assert.ok(!(await (await page('/task-log?filter=pending')).text()).includes(demoTaskPath));
const julyFormal = await recoverImported(await fetch(origin + root + '/create', {
  method: 'POST', headers: { Origin: origin }, redirect: 'manual',
  body: new URLSearchParams({
    industry: '食品饮料', budget: '200', bus: '1', metro: '1', ...sourceDate, brief_campaign_goal: '认知', brief_confirmed: '1'
  }),
}), '200');
assert.equal(julyFormal.status, 303);
const julyFormalPath = julyFormal.headers.get('location');
const awaitingOps = await completedTask(julyFormalPath);
assert.equal(awaitingOps.state, 'review');
assert.equal(awaitingOps.offer_review, false);
assert.deepEqual(awaitingOps.approval_readiness, { a: false, b: false });
assert.match(awaitingOps.steps[4].notes.join(' '), /A 案核验状态.*B 案核验状态/u);
assert.match(awaitingOps.steps[4].notes.join(' '), /待核 · 上下刊施工/u);
const readyForReview = await fetch(origin + julyFormalPath + '/operations', {
  method: 'POST', headers: { Origin: origin }, redirect: 'manual',
  body: new URLSearchParams({
    ops_source: '隔离回归条件', ops_hours: '22:00-05:00', ops_bus: '2', ops_metro: '1', ops_buffer: '1', ops_confirmed: '1',
    ops_windows: '2027-07-01 | 2027-06-24 至 2027-06-28 | 2027-08-01 至 2027-08-03',
  }),
});
assert.equal(readyForReview.status, 303, await readyForReview.text());
assert.equal((await completedTask(readyForReview.headers.get('location'))).state, 'done');
const busOnly = new FormData();
for (const [name, value] of Object.entries({ ...input, observed_at: '2026-11-21T10:20' })) busOnly.set(name, value);
file(busOnly, 'bus_stock', '线路名称,行车公司,线路等级,媒体名称,空车数\n99,隔离测试,A+,大三侧,1\n');
file(busOnly, 'bus_coverage', '线路名称,目标圈,半径\n99,单公交测试圈,300\n');
file(busOnly, 'rates', '媒体类型,匹配对象,发布费,计价数量,计价周期,制作费\n公交车,A+,约3–5万,1,1月,约5–10千\n');
const moreRecent = await fetch(origin + '/api/media-proposals/import', {
  method:'POST', headers:{Origin:origin}, body:busOnly, redirect:'manual',
});
assert.equal(moreRecent.status,303);
const evidenceDraft = await run({ industry: 'food', month: '2027-07', media: 'bus', budget: '200' });
assert.equal(evidenceDraft.status, 303);
const evidenceDraftPath = await confirmTaskGoal(evidenceDraft.headers.get('location'));
const evidenceState = await completedTask(evidenceDraftPath);
assert.equal(evidenceState.state, 'review');
assert.equal(evidenceState.offer_local_snapshot, true);
const allTasks = await (await page('/task-log')).text();
assert.match(allTasks, /<option value="exceptions">异常任务<\/option>/u);
const scopedExceptions = await (await page('/task-log?filter=exceptions&q=' + evidenceDraftPath.split('/').at(-1))).text();
assert.ok(scopedExceptions.includes(`href="${evidenceDraftPath}"`));
assert.match(scopedExceptions, /data-status="待核对"/u);
const pendingTasks = await (await page('/task-log?filter=pending')).text();
assert.ok(pendingTasks.includes(`href="${taskPath}"`));
assert.ok(!pendingTasks.includes(`href="${evidenceDraftPath}"`));
const draftReport = await (await page(evidenceDraftPath + '/review-copy?preview=1')).text();
assert.match(draftReport, /待补新云触达数据/u);
assert.match(await (await page(evidenceDraftPath)).text(), /查看补证清单/u);
const checklist = await (await page(evidenceDraftPath + '/evidence')).text();
assert.match(checklist, /2027-07 缺少 人次\/辆\/天/u);
assert.match(checklist, /A／B/u);
assert.match(checklist, /补充本档期证据/u);
assert.doesNotMatch(checklist, /人次\/点位\/天/u);
const selectedEvidence = '/evidence?starts=2027-07-01&variant=B&kind=reach';
const filteredChecklist = await (await page(evidenceDraftPath + selectedEvidence)).text();
assert.match(filteredChecklist, /显示 1 项，共 [1-9][0-9]* 项/u);
assert.match(filteredChecklist, /<th scope="row">单位日触达<\/th>/u);
assert.doesNotMatch(filteredChecklist, /<th scope="row">(公交轨迹|新云线路原图)<\/th>/u);
const filteredCsv = await (await page(evidenceDraftPath + selectedEvidence + '&download=1')).text();
assert.equal(filteredCsv.trimEnd().split('\r\n').length, 2);
assert.match(filteredCsv, /"A／B","单位日触达","99"/u);
assert.match(await (await page(evidenceDraftPath + '/evidence?kind=exit')).text(),
  /当前筛选条件没有匹配的证据缺项/u);
await page(evidenceDraftPath + '/evidence?variant=C', 400);
await page(evidenceDraftPath + '/evidence?starts=2027-06-01', 400);
const checklistCsv = await page(evidenceDraftPath + '/evidence?download=1');
assert.match(checklistCsv.headers.get('content-disposition'), /attachment; filename=cityline-evidence.csv/u);
assert.match(await checklistCsv.text(), /"单位日触达","99"/u);
const refreshEvidence = () => fetch(origin + evidenceDraftPath + '/rerun', {
  method: 'POST', headers: { Origin: origin }, redirect: 'manual',
  body: new URLSearchParams({ budget: '200', local_snapshot: '1' }),
});
const reviewSameInventory = () => fetch(origin + evidenceDraftPath + '/rerun', {
  method: 'POST', headers: { Origin: origin }, redirect: 'manual',
  body: new URLSearchParams({ budget: '200', same_inventory: '1' }),
});
assert.equal((await refreshEvidence()).status, 409);
assert.equal((await reviewSameInventory()).status, 409);
const fixtureAugment = new FormData();
fixtureAugment.set('attested', 'yes');
fixtureAugment.set('route_geometry', new Blob([JSON.stringify({
  type: 'FeatureCollection', features: [{ type: 'Feature',
    properties: { media: 'bus', resource: '99路', source_ref: '隔离测试坐标' },
    geometry: { type: 'LineString', coordinates: [[121.44, 31.21], [121.48, 31.25]] },
  }],
})], { type: 'application/geo+json' }), 'test-recovered-route.geojson');
const busSnapshot = new URL(moreRecent.headers.get('location'), origin).searchParams.get('batch');
const evidencePath = '/api/media-proposals/snapshots/' + busSnapshot + '/evidence';
const evidenceTaskId = evidenceDraftPath.split('/').at(-1);
assert.ok(checklist.includes(`?reach_template=${busSnapshot}`));
await page(evidenceDraftPath + '/evidence?reach_template=00000000-0000-4000-8000-000000000000', 404);
const templateResponse = await page(evidenceDraftPath + '/evidence?reach_template=' + busSnapshot);
assert.match(templateResponse.headers.get('content-disposition'), /attachment; filename=cityline-reach-2027-07-01.csv/u);
const reachTemplate = await templateResponse.text();
assert.deepEqual(reachTemplate.trimEnd().split('\r\n').slice(1), [
  '"公交车身","99","2023-07","","人次/辆/天","",""',
]);
const blankReach = new FormData();
blankReach.set('attested', 'yes');
file(blankReach, 'reach', reachTemplate);
assert.equal((await fetch(origin + evidencePath, {
  method: 'POST', headers: { Origin: origin }, body: blankReach, redirect: 'manual',
})).status, 422);
file(blankReach, 'reach', reachTemplate.replace('"2023-07","","人次/辆/天"', '"2023-07","1234","人次/辆/天"'));
assert.equal((await fetch(origin + evidencePath, {
  method: 'POST', headers: { Origin: origin }, body: blankReach, redirect: 'manual',
})).status, 422);
const filledReach = reachTemplate.replace(
  '"2023-07","","人次/辆/天","",""', '"2023-07","1234","人次/辆/天","测试补录原行",""',
) + '"公交车身","99","2023-08","","人次/辆/天","",""\r\n';
file(fixtureAugment, 'reach', filledReach);
assert.deepEqual(evidenceState.recovery_snapshots, [{ id: busSnapshot, starts: '2027-07-01', ends: '2027-07-31' }]);
assert.equal(evidenceState.offer_same_inventory, true);
const pinnedUrl = '/media-data?batch=' + busSnapshot + '&task=' + evidenceTaskId;
const evidenceDetail = await (await page(evidenceDraftPath)).text();
assert.ok(evidenceDetail.includes(pinnedUrl.replace('&', '&amp;')));
const pinnedPage = await (await page(pinnedUrl)).text();
assert.equal((pinnedPage.match(/class="snapshot-row"/gu) ?? []).length, 1);
assert.match(pinnedPage, /<details class="import-options" open>/u);
assert.ok(pinnedPage.includes('action="' + evidencePath + '?task=' + evidenceTaskId + '"'));
assert.doesNotMatch(pinnedPage, /id="supplement-review"/u);
const unrelatedTaskId = taskPath.split('/').at(-1);
await page('/media-data?batch=' + busSnapshot + '&task=' + unrelatedTaskId, 404);
const unrelatedUpload = await fetch(origin + evidencePath + '?task=' + unrelatedTaskId, {
  method: 'POST', headers: { Origin: origin }, body: fixtureAugment, redirect: 'manual',
});
assert.equal(unrelatedUpload.status, 404);
const augmented = await fetch(origin + evidencePath + '?task=' + evidenceTaskId, {
  method: 'POST', headers: { Origin: origin }, body: fixtureAugment, redirect: 'manual',
});
assert.equal(augmented.status, 303, await augmented.text());
const augmentedUrl = new URL(augmented.headers.get('location'), origin);
assert.equal(augmentedUrl.searchParams.get('task'), evidenceTaskId);
assert.notEqual(augmentedUrl.searchParams.get('batch'), busSnapshot);
const augmentedPage = await (await page(augmentedUrl.pathname + augmentedUrl.search)).text();
assert.ok(augmentedPage.includes(`href="${evidenceDraftPath}">返回原提案任务</a>`));
assert.match(augmentedPage, /id="supplement-review"/u);
assert.ok(augmentedPage.includes(`action="${evidenceDraftPath}/rerun"`));
assert.match(await (await page(pinnedUrl)).text(), /id="supplement-review"/u);
const again = await fetch(origin + evidencePath + '?task=' + evidenceTaskId, {
  method: 'POST', headers: { Origin: origin }, body: fixtureAugment, redirect: 'manual',
});
assert.equal(again.headers.get('location'), augmented.headers.get('location'));
const sourceAudit = await (await page('/audit?task=' + evidenceTaskId)).text();
assert.equal((sourceAudit.match(/>补充线路和触达证据<\/td>/gu) ?? []).length, 1);
const recovered = await reviewSameInventory();
assert.equal(recovered.status, 303, await recovered.text());
const recoveredPath = recovered.headers.get('location');
assert.notEqual(recoveredPath, evidenceDraftPath);
const recoveredState = await completedTask(recoveredPath);
assert.equal(recoveredState.state, 'done');
assert.match(recoveredState.steps[0].notes.join(' '), /已沿用原 A／B 采购清单和报价/u);
const recoveredReport = await (await page(recoveredPath + '/review-copy?preview=1')).text();
const recoveredOriginalSnapshotUrl = '/media-data?batch=' + busSnapshot + '&task=' + recoveredPath.split('/').at(-1);
assert.doesNotMatch(await (await page(recoveredOriginalSnapshotUrl)).text(), /id="supplement-review"/u);
assert.doesNotMatch(await (await page(recoveredPath + '/evidence')).text(), /2027-07 缺少 人次\/辆\/天/u);
await page(recoveredPath + '/evidence?reach_template=' + augmentedUrl.searchParams.get('batch'), 409);
assert.match(recoveredReport, /38,254 人次/u);
const financialRows = (html) => [...html.matchAll(
  /<tr>\s*<th scope="row">([^<]+)<small>[^<]*<\/small>[\s\S]*?<\/th>\s*((?:<td>[^<]*<\/td>){5})/gu,
)].map(([, resource, financials]) => [resource, financials]);
assert.equal(financialRows(draftReport).length, 2);
assert.deepEqual(financialRows(recoveredReport), financialRows(draftReport));
assert.equal(await (await page(evidenceDraftPath + '/review-copy?preview=1')).text(), draftReport);
const postReview = (decision, note, variant = '', acceptGap = '0') => fetch(origin + recoveredPath + '/review', {
  method: 'POST', headers: { Origin: origin }, redirect: 'manual',
  body: new URLSearchParams({ decision, note, variant, accept_budget_gap: acceptGap }),
});
assert.equal((await postReview('returned', '')).status, 422);
assert.equal((await postReview('approved', '核验恢复完成')).status, 422);
assert.equal((await postReview('approved', '核验恢复完成', 'C')).status, 422);
assert.equal((await postReview('approved', '未确认预算偏差', 'A')).status, 422);
assert.equal((await postReview('approved', '', 'A', '1')).status, 422);
const scopedAuditUrl = '/audit?task=' + recoveredPath.split('/').at(-1);
if (process.env.CITYLINE_TEST_REDBEAN_APPROVAL === '1') {
  const concurrent = await Promise.all([
    postReview('approved', '核验恢复完成', 'A', '1'), postReview('approved', '并发复核', 'B', '1'),
  ]);
  assert.equal(concurrent.filter(r => r.status === 303).length, 1);
  assert.ok(concurrent.some(r => r.status === 409 || r.status === 429));
  const adoptedVariant = concurrent[0].status === 303 ? 'A' : 'B';
  assert.equal((await completedTask(recoveredPath)).approved_variant, adoptedVariant);
  const approvedResponse = await page(recoveredPath + '/review-copy?preview=1');
  const adoptedReport = await approvedResponse.text();
  assert.ok(adoptedReport.includes(`data-selected-variant="${adoptedVariant}">采用方案 ${adoptedVariant}`));
  assert.doesNotMatch(adoptedReport, workflowCopy);
  assert.ok(adoptedReport.includes(`id="report-view-${adoptedVariant === 'A' ? 1 : 2}" checked`));
  assert.doesNotMatch(adoptedReport, /id="report-view-all" checked/u);
  await page(recoveredPath + '/report?delivery=1', 409);
  const issued = adoptedReport;
  assert.ok(issued.includes(`data-selected-variant="${adoptedVariant}"`));
  assert.match(issued, generatedAt);
  assert.doesNotMatch(issued,/核验恢复完成|并发复核/u);
  const recoveredPage = await (await page(recoveredPath)).text();
  assert.ok(recoveredPage.includes(evidenceDraftPath));
  assert.ok(recoveredPage.includes(`采用方案 ${adoptedVariant}`));
  assert.match(recoveredPage,/导出已确认方案 HTML/u);
  assert.ok(recoveredPage.includes(`href="${scopedAuditUrl}">查看任务审计</a>`));
  assert.match(recoveredPage, /审核意见/u);
  assert.doesNotMatch(recoveredPage, /name="decision"/u);
  const scopedAudit = await (await page(scopedAuditUrl)).text();
  assert.match(scopedAudit, /审批库存复核通过/u);
  assert.match(scopedAudit, /已确认预算偏差/u);
  assert.ok(scopedAudit.includes(`采用方案 ${adoptedVariant}`));
  assert.ok(!scopedAudit.includes(`href="${evidenceDraftPath}"`));
} else {
  const noAuthorization = await postReview('approved', '离线复核', 'A', '1');
  assert.equal(noAuthorization.status, 409);
  assert.match(await noAuthorization.text(), /完成红豆授权/u);
  const unapproved = await completedTask(recoveredPath);
  assert.equal(unapproved.approved_variant, null);
  assert.match(unapproved.approval_inventory_issue, /完成红豆授权/u);
  assert.match(await (await page(recoveredPath)).text(), /库存复核待处理/u);
  await page(recoveredPath + '/report?delivery=1', 409);
  assert.doesNotMatch(await (await page(recoveredPath+'/review-copy?preview=1')).text(),workflowCopy);
  const inventoryExceptions = '/task-log?filter=exceptions&q=' + recoveredPath.split('/').at(-1);
  assert.match(await (await page(inventoryExceptions)).text(), /data-status="库存复核待处理"/u);
  const blockedQueues = await overviewQueues();
  assert.ok(queueSection(blockedQueues,'exception-tasks').includes(`href="${recoveredPath}"`));
  assert.ok(!queueSection(blockedQueues,'ready-tasks').includes(`href="${recoveredPath}"`));
  assert.ok(!(await (await page('/task-log?filter=pending&q=' + recoveredPath.split('/').at(-1))).text())
    .includes(`href="${recoveredPath}"`));
}
const recoveryAudit = await (await page('/audit')).text();
assert.match(recoveryAudit, /重新生成/u);
assert.match(recoveryAudit, /补证复核/u);
const stillSnapshot = await page(location.replace(root + '?', root + '/snapshot-review?'));
assert.equal(stillSnapshot.headers.get('content-disposition'), null);
await page(location.replace(root + '?', root + '/export?'), 409);
const bothStill = await stillSnapshot.text();
assert.match(bothStill, /¥221000–371000/u);
const busCatalog = await(await page('/media-data?source=local&starts=2027-07-01&ends=2027-07-31')).text();
const newestBus = busCatalog.match(/media=bus&amp;starts=2027-07-01&amp;ends=2027-07-31&amp;batch=([a-f0-9-]{36})/u)?.[1];
assert.ok(newestBus && newestBus !== busSnapshot);
const freshestBus = await(await page(root + '/snapshot-review?industry=food&starts=2027-07-01&ends=2027-07-31&media=bus&batch=' + newestBus)).text();
assert.match(freshestBus,/单公交测试圈/u);
busOnly.set('observed_at', '2026-11-21T10:25');
file(busOnly, 'bus_stock', '线路名称,行车公司,线路等级,媒体名称,空车数\n99,隔离测试,A+,大三侧,2\n');
const newStock = await fetch(origin + '/api/media-proposals/import', {
  method: 'POST', headers: { Origin: origin }, body: busOnly, redirect: 'manual',
});
assert.equal(newStock.status, 303, await newStock.text());
const retained = await reviewSameInventory();
assert.equal(retained.status, 303, await retained.text());
const retainedPath = retained.headers.get('location');
assert.equal((await completedTask(retainedPath)).state, 'done');
const retainedReport = await (await page(retainedPath + '/review-copy?preview=1')).text();
assert.deepEqual(financialRows(retainedReport), financialRows(draftReport));
assert.match(retainedReport, /38,254 人次/u);
const retainedReturn = await fetch(origin + retainedPath + '/review', {
  method: 'POST', headers: { Origin: origin }, redirect: 'manual',
  body: new URLSearchParams({ decision: 'returned', note: '隔离测试退回原因' }),
});
assert.equal(retainedReturn.status, 303);
const retainedId = retainedPath.split('/').at(-1);
const returnedTasks = await (await page('/task-log?filter=exceptions&q=' + retainedId)).text();
assert.match(returnedTasks, /data-status="已退回"/u);
const deleted = await fetch(origin + retainedPath + '/delete', {
  method: 'POST', headers: { Origin: origin }, redirect: 'manual',
});
assert.equal(deleted.status, 303);
await page(retainedPath, 404);
const retainedAudit = await (await page('/audit?task=' + retainedId)).text();
assert.match(retainedAudit, /title="删除">删除/u);
assert.match(retainedAudit, /隔离测试退回原因/u);
assert.match(retainedAudit, /仅保留审计/u);
assert.ok(!retainedAudit.includes(`href="${retainedPath}"`));
const replanned = await refreshEvidence();
assert.equal(replanned.status, 303, await replanned.text());
const replannedPath = replanned.headers.get('location');
const replannedState = await completedTask(replannedPath);
assert.equal(replannedState.state, 'review');
assert.doesNotMatch(replannedState.steps[0].notes.join(' '), /已沿用原 A／B 采购清单和报价/u);
assert.match(await (await page(replannedPath + '/review-copy?preview=1')).text(), /<td>2 \/ 2<\/td>/u);
const recalculated = await recoverImported(await fetch(origin + taskPath + '/rerun', {
  method: 'POST', headers: { Origin: origin }, redirect: 'manual',
  body: new URLSearchParams({ budget: '150' }), signal: AbortSignal.timeout(10000),
}), '150');
assert.equal(recalculated.status, 303);
const recalculatedPath = recalculated.headers.get('location');
const rebudgeted = await completedTask(recalculatedPath);
assert.equal(rebudgeted.state, 'done');
assert.match(rebudgeted.summary, /150 万元/u);
assert.match(await (await page(recalculatedPath + '/review-copy')).text(), /与 200 万预算方案比较/u);
const crossPeriod = await importData({ ...input, starts: '2027-09-25', ends: '2027-10-06' });
assert.equal(crossPeriod.status, 303);
const periodResult = await (await page(crossPeriod.headers.get('location'))).text();
assert.match(periodResult, /覆盖的 2 个自然月估算/u);
assert.match(periodResult, /投放 2027-09-25 至 2027-10-06/u);
for (const [starts, ends] of [['2027-09-16', '2027-10-15'], ['2027-10-16', '2027-10-20']]) {
  assert.equal((await importData({ ...input, starts, ends })).status, 303);
}
const rolling = await recoverImported(await fetch(origin + root + '/create', {
  method: 'POST', headers: { Origin: origin }, redirect: 'manual',
  body: new URLSearchParams({
    industry: '食品饮料', starts: '2027-01-01', ends: '2027-12-31',
    budget: '200', bus: '1', metro: '1',
    scheduling_factors: ['2027-05-01 | 1 | 预热', '2027-07-01 | 3 | 主投', '2027-10-01 | 3 | 测试活动档期'].join('\n'),
  }),
}), '200');
assert.equal(rolling.status, 303, await rolling.text());
const rollingPath = rolling.headers.get('location');
assert.equal((await completedTask(rollingPath)).state, 'review');
const rollingHtml = await (await page(rollingPath + '/review-copy')).text();
verifyCampaignRequirements(rollingHtml);
assert.match(rollingHtml, /2027-01-01 至 2027-12-31/u);
assert.match(rollingHtml, /2027-10-01 至 2027-10-31<\/strong>\s*<p>31 天 · 待核对/u);
assert.match(rollingHtml, generatedAt);
assert.match(rollingHtml, /测试活动档期 · 调配权重 3/u);
assert.match(rollingHtml, /机动 · 无采买/u);
assert.match(rollingHtml, /预算已预留/u);
assert.match(rollingHtml, /品牌与投放目标/u);
const catalog = await (await page('/media-data?source=local')).text();
assert.match(catalog, /核对快照/u);
assert.ok(catalog.includes(new URL(origin + location).searchParams.get('batch')));
assert.ok(!(await (await page('/media-data?source=platform')).text())
  .includes(new URL(origin + location).searchParams.get('batch')));
const unavailable = await run({industry: 'food', month: '2027-08', media: 'both', budget: '200'});
assert.equal(unavailable.status, 303);
const blocked = await completedTask(unavailable.headers.get('location'));
assert.equal(blocked.state, 'review');
assert.match(blocked.steps[1].notes.join(' '), /本机尚未设置赛题刊例路径/u);
assert.equal((await fetch(origin + unavailable.headers.get('location') + '/report')).status, 409);
const unaffordable = await run({
  industry: 'food', month: '2027-07', media: 'both', budget: '200', hard_cap: '1',
});
assert.equal(unaffordable.status, 303, await unaffordable.text());
const unaffordablePath = await confirmTaskGoal(unaffordable.headers.get('location'));
const unaffordableTask = await completedTask(unaffordablePath);
assert.equal(unaffordableTask.state, 'review');
assert.equal(unaffordableTask.offer_review, false);
const unaffordableReport = await (await page(unaffordablePath + '/review-copy?preview=1')).text();
assert.doesNotMatch(unaffordableReport, workflowCopy);
assert.match(unaffordableReport, /最低组合超过客户硬上限/u);
assert.match(unaffordableReport, /本次没有通过全部条件的媒体/u);

const brief = new FormData();
const retiredBrand = String.fromCodePoint(0x661f, 0x5df4, 0x514b);
const trialInput = {
  trial_stores: '静安脱敏门店组 A01–A04',
  trial_window: '各投放段营业日起止，按北京时间分周汇总',
  trial_baseline: '同店上线前等长营业期，匹配相同星期与营业时段',
};
brief.set('text', `主题：${retiredBrand}门店试饮\n行业：食品饮料\n投放预算：200万元\n媒体形式：公交车身及地铁出入口\n投放周期：2027年7月\n指定线路：10路、11路\n投放目标：门店试饮活动\n脱敏客户规模：上海区域连锁，已核对12处门店\n主目标：到店\n首要比选指标：核心圈公交车日\n目标人群：工作日午间通勤者\n试饮门店范围：${trialInput.trial_stores}\n试饮扫码统计窗口：${trialInput.trial_window}\n试饮扫码基线：${trialInput.trial_baseline}`);
const parsedBrief = await fetch(origin + '/api/media-proposals/brief', { method: 'POST', headers: { Origin: origin }, body: brief });
assert.equal(parsedBrief.status, 200);
const extracted = await parsedBrief.json();
assert.equal(extracted.subject, '食品饮料品牌门店试饮');
assert.deepEqual(extracted.required_routes, ['10路', '11路']);
assert.equal(extracted.starts, '2027-07-01');
assert.equal(extracted.ends, '2027-07-31');
assert.deepEqual(extracted.issues, []);
assert.equal(extracted.brief.customer_profile, '上海区域连锁，已核对12处门店');
assert.equal(extracted.brief.campaign_goal, '到店');
assert.equal(extracted.brief.comparison_focus, 'core_bus_days');
assert.equal(extracted.brief.audience, '工作日午间通勤者');
assert.deepEqual(extracted.brief.trial_measurement, {
  stores: trialInput.trial_stores, window: trialInput.trial_window, baseline: trialInput.trial_baseline,
});

async function createWithRoutes(routes) {
  const response = await recoverImported(await fetch(origin + root + '/create', {
    method: 'POST', headers: { Origin: origin }, redirect: 'manual',
    body: new URLSearchParams({
      industry: '食品饮料', starts: '2027-01-01', ends: '2027-12-31',
      budget: '200', bus: '1', metro: '1', required_routes: routes,
      brief_campaign_goal: '认知', brief_confirmed: '1',
      scheduling_factors: ['2027-05-01 | 1 | 预热', '2027-07-01 | 3 | 主投', '2027-10-01 | 1 | 回访'].join('\n'),
    }),
  }), '200');
  assert.equal(response.status, 303);
  const path = response.headers.get('location');
  return { path, job: await completedTask(path) };
}
const selectedRoute = await createWithRoutes('10路');
assert.equal(selectedRoute.job.state, 'review');
assert.match(selectedRoute.job.steps[4].notes.join(' '), /由本地规则执行报告核验 Agent/u);
const selectedReport = await (await page(selectedRoute.path + '/review-copy')).text();
assert.match(selectedReport, /10路 · 已核对档期可售余量与刊例/u);
const missingSnapshotReplacements = await (await page(selectedRoute.path + '/replacements')).text();
assert.match(missingSnapshotReplacements, /需先补齐所有采购段的精确档期快照/u);
assert.doesNotMatch(missingSnapshotReplacements, /name="alternative"/u);
assert.match(selectedReport, /目标人群/u);
assert.equal((selectedReport.match(/整案预算量级<\/dt><dd>约 200 万元<\/dd>/gu) ?? []).length, 1);
assert.ok(!selectedReport.includes(retiredBrand));
const forbiddenBudget = await fetch(origin + selectedRoute.path + '/rerun', {
  method: 'POST', headers: { Origin: origin }, body: new URLSearchParams({ budget: '500' }),
});
assert.equal(forbiddenBudget.status, 422);
const evaluationResponse = await fetch(origin + selectedRoute.path + '/rerun', {
  method: 'POST', headers: { Origin: origin }, redirect: 'manual',
  body: new URLSearchParams({ budget: '100', evaluation: '1' }),
});
assert.equal(evaluationResponse.status, 303);
const evaluationPath = evaluationResponse.headers.get('location');
const evaluationProgress = await completedTask(evaluationPath);
assert.equal(evaluationProgress.offer_evaluation, true);
const evaluationHtml = await (await page(evaluationPath + '/review-copy')).text();
assert.match(evaluationHtml, /2027-01-01 至 2027-12-31/u);
assert.match(evaluationHtml, /整案预算量级<\/dt><dd>约 100 万元/u);
assert.match(evaluationHtml, /对照方案报价上限/u);
assert.match(evaluationHtml, /预算变化 · 200 万元 → 100 万元/u);
assert.deepEqual([...evaluationHtml.matchAll(/data-budget-strategy="([AB])"/gu)].map(match => match[1]), ['A', 'B']);
assert.match(evaluationHtml, /两版金额均为预留额度/u);
assert.match(evaluationHtml, /保留资源/u);

const followupResponse = await fetch(origin + evaluationPath + '/rerun', {
  method: 'POST', headers: { Origin: origin }, redirect: 'manual',
  body: new URLSearchParams({ budget: '500', evaluation: '1' }),
});
assert.equal(followupResponse.status, 303, await followupResponse.text());
const followupPath = followupResponse.headers.get('location');
const followup = await completedTask(followupPath);
assert.equal(followup.offer_evaluation, true);
assert.equal(followup.login.phase, '');
const followupHtml = await (await page(followupPath + '/review-copy')).text();
assert.match(followupHtml, /预算变化 · 100 万元 → 500 万元/u);
assert.doesNotMatch(followupHtml, /预算变化 · 200 万元 → 500 万元/u);
assert.deepEqual([...followupHtml.matchAll(/data-budget-strategy="([AB])"/gu)].map(match => match[1]), ['A', 'B']);

const draft = await createWithRoutes('11路、12路');
assert.equal(draft.job.state, 'review');
assert.equal(draft.job.has_alternative, true);
const draftResponse = await page(draft.path + '/review-copy?preview=1');
const draftAnnual = await draftResponse.text();
assert.match(draftAnnual, /投放条件说明/u);
assert.doesNotMatch(draftAnnual, workflowCopy);
assert.match(draftAnnual, /11路 · 红豆记录显示该档期无可售余量/u);
assert.match(draftAnnual, /12路 · 红豆同档期快照无该线路记录/u);
assert.match(draftAnnual, /整案最低报价待核对/u);
assert.doesNotMatch(draftAnnual, /最低高位报价合计 <strong>/u);
const approveDraft = await fetch(origin + draft.path + '/review', {
  method: 'POST', headers: { Origin: origin }, body: new URLSearchParams({ decision: 'approved', note: '' }),
});
assert.equal(approveDraft.status, 409);
const exported = await page(location.replace(root + '?', root + '/snapshot-review?'));
assert.match(await exported.text(), /上刊准备/u);
const wrong = await (await page(root + '?industry=food&starts=2027-08-01&ends=2027-08-31')).text();
assert.match(wrong, /该档期没有可查看的获取数据记录/u);
assert.doesNotMatch(wrong, /¥221000–371000/u);
const mayImport = await importData({ ...input, starts: '2027-05-01', ends: '2027-05-31' }, false);
assert.equal(mayImport.status, 303, await mayImport.text());
assert.equal((await importData({ ...input, starts: '2027-10-01', ends: '2027-10-31' })).status, 303);
const annualDraft = await createWithRoutes('10路');
assert.equal(annualDraft.job.state, 'review');
assert.deepEqual(annualDraft.job.recovery_snapshots.map(({ starts }) => starts), ['2027-05-01', '2027-07-01', '2027-10-01']);
const annualDraftReport = await (await page(annualDraft.path + '/review-copy?preview=1')).text();
assert.equal(financialRows(annualDraftReport).length, 12);
assert.match(annualDraftReport, /本段施工待核/u);
const ops = {
  ops_source: '仅供隔离测试的施工排班', ops_hours: '22:00-05:00', ops_bus: '2', ops_metro: '1', ops_buffer: '1',
  ops_windows: '2027-05-01 | 2027-04-28 至 2027-04-28 | 2027-06-01 至 2027-06-02',
};
const reviewOps = (confirmed) => fetch(origin + annualDraft.path + '/operations', {
  method: 'POST', headers: { Origin: origin }, redirect: 'manual',
  body: new URLSearchParams({ ...ops, ops_confirmed: String(confirmed) }),
});
assert.equal((await reviewOps(0)).status, 422);
const plannedOps = await reviewOps(1);
assert.equal(plannedOps.status, 303, await plannedOps.text());
const opsPath = plannedOps.headers.get('location');
const opsStatus = await completedTask(opsPath);
assert.equal(opsStatus.state, 'review');
assert.match(opsStatus.steps[0].notes.join(' '), /本次复核施工窗口与产能/u);
assert.deepEqual(opsStatus.recovery_snapshots, annualDraft.job.recovery_snapshots);
const opsHtml = await (await page(opsPath + '/review-copy?preview=1')).text();
assert.match(opsHtml, /上刊 2027-04-28 至 2027-04-28 · 可用 1 晚 · 含缓冲还缺 2 晚/u);
assert.match(opsStatus.steps[4].notes.join(' '), /待核 · 上下刊施工/u);
assert.match(opsHtml, /<dt>上下刊施工<\/dt>/u);
assert.deepEqual(financialRows(opsHtml), financialRows(annualDraftReport));
const maySnapshot = new URL(mayImport.headers.get('location'), origin).searchParams.get('batch');
const mayReach = new FormData();
mayReach.set('attested', 'yes');
file(mayReach, 'reach', '媒体类型,资源编号或线路,统计月份,每日触达人次,计量单位,来源记录\n公交车身,10路,2023-05,1386,人次/辆/天,测试五月原行');
const mayAugmented = await fetch(origin + `/api/media-proposals/snapshots/${maySnapshot}/evidence`, {
  method: 'POST', headers: { Origin: origin }, body: mayReach, redirect: 'manual',
});
assert.equal(mayAugmented.status, 303, await mayAugmented.text());
const annualRecovery = await fetch(origin + annualDraft.path + '/rerun', {
  method: 'POST', headers: { Origin: origin }, redirect: 'manual',
  body: new URLSearchParams({ budget: '200', same_inventory: '1' }),
});
assert.equal(annualRecovery.status, 303, await annualRecovery.text());
const annualRecoveryPath = annualRecovery.headers.get('location');
const annualRecoveryState = await completedTask(annualRecoveryPath);
assert.equal(annualRecoveryState.state, 'review');
assert.match(annualRecoveryState.steps[0].notes.join(' '), /已沿用原 A／B 采购清单和报价/u);
const annualRecoveryReport = await (await page(annualRecoveryPath + '/review-copy?preview=1')).text();
assert.match(annualRecoveryReport, /1,386 人次\/辆\/天/u);
assert.deepEqual(financialRows(annualRecoveryReport), financialRows(annualDraftReport));
const focusDates = { starts: '2027-08-01', ends: '2027-08-31' };
const firstCenter = '上海市静安区验收地标甲';
const secondCenter = '上海市徐汇区验收地标乙';
async function importFocusedData(includesFirst) {
  const form = new FormData();
  const observed_at = includesFirst ? '2026-11-21T10:40' : '2026-11-21T10:45';
  for (const [field, value] of Object.entries({ ...input, ...focusDates, observed_at })) form.set(field, value);
  file(form, 'bus_stock', '线路名称,行车公司,线路等级,媒体名称,空车数\n10,测试运营方,A+,大三侧,1\n12,测试运营方,A+,大三侧,1');
  file(form, 'bus_coverage', `线路名称,目标圈,半径\n${includesFirst ? '10,' + firstCenter + ',1500\n' : ''}12,${secondCenter},1500`);
  file(form, 'metro_stock', '点位名称,站点等级,媒体形式,可售数量,面积\n演示站A号口,A+,灯箱,1,9.1');
  file(form, 'metro_coverage', `点位名称,目标圈,半径\n${includesFirst ? '演示站A号口,' + firstCenter + ',1500\n' : ''}演示站A号口,${secondCenter},1500`);
  file(form, 'rates', '媒体类型,匹配对象,发布费,计价数量,计价周期,制作费\n公交车,A+,约3–5万,1,1月,约5–10千\n地铁出入口,A+,约15–25万,1,1月,100元/㎡');
  const result = await fetch(origin + '/api/media-proposals/import', { method: 'POST', headers: { Origin: origin }, body: form, redirect: 'manual' });
  assert.equal(result.status, 303, await result.text());
  return new URL(result.headers.get('location'), origin).searchParams.get('batch');
}
const twoCenters = await importFocusedData(true);
const newerSecondCenter = await importFocusedData(false);
const createFocused = (centers, confirmed, core = '') => fetch(origin + root + '/create', {
  method: 'POST', headers: { Origin: origin }, redirect: 'manual',
  body: new URLSearchParams({
    industry: '食品饮料', budget: '200', bus: '1', metro: '1', ...focusDates, target_centers: centers,
    core_circle: core,
    brief_customer_profile: extracted.brief.customer_profile, brief_objective: '验证午间试饮需求',
    brief_campaign_goal: '到店', brief_audience: extracted.brief.audience,
    brief_region: '门店午休动线', brief_monitoring: '各门店按周统计试饮扫码', brief_confirmed: String(confirmed),
    ...trialInput,
  }),
});
assert.equal((await createFocused(firstCenter, 0)).status, 422);
assert.equal((await createFocused('无效地标', 1)).status, 422);
assert.equal((await createFocused(firstCenter, 1, secondCenter)).status, 422);
for (const [center, batch, route] of [[firstCenter, twoCenters, '10'], [secondCenter, newerSecondCenter, '12']]) {
  const started = await recoverImported(await createFocused(center, 1), '200');
  assert.equal(started.status, 303);
  const scopedPath = started.headers.get('location');
  const scoped = await completedTask(scopedPath);
  assert.deepEqual(scoped.recovery_snapshots, [{ id: batch, ...focusDates }]);
  const html = await (await page(scopedPath + '/review-copy?preview=1')).text();
  const sheetResponse = await page(scopedPath + '/campaign');
  assert.match(sheetResponse.headers.get('content-disposition'), /cityline-campaign\.html/u);
  const sheet = await sheetResponse.text();
  for (const value of [center, extracted.brief.customer_profile, extracted.brief.audience, '门店午休动线', '各门店按周统计试饮扫码']) {
    assert.ok(html.includes(value) && sheet.includes(value), value);
  }
  assert.match(html, /到店试饮有效扫码量。投放后按匿名事件去重/u);
  for (const text of Object.values(trialInput)) {
    assert.ok(html.includes(text) && sheet.includes(text), text);
  }
  assert.equal(financialRows(html).filter(([resource]) => ['10', '12'].includes(resource)).length, 2);
  assert.ok(financialRows(html).filter(([resource]) => ['10', '12'].includes(resource)).every(([resource]) => resource === route));
}
const explicitCore = await recoverImported(
  await createFocused([firstCenter, secondCenter].join('\n'), 1, secondCenter), '200',
);
assert.equal(explicitCore.status, 303);
const explicitCorePath = explicitCore.headers.get('location');
const explicitCoreState = await completedTask(explicitCorePath);
assert.deepEqual(explicitCoreState.recovery_snapshots, [{ id: twoCenters, ...focusDates }]);
assert.ok(explicitCoreState.steps[0].notes.join(' ').includes(`已指定核心目标圈 · ${secondCenter}`));
const explicitCoreReport = await (await page(explicitCorePath + '/review-copy?preview=1')).text();
assert.ok(explicitCoreReport.includes(`核心目标圈 · ${secondCenter}`));

const goalDraft = await recoverImported(await fetch(origin + root + '/create', {
  method: 'POST', headers: { Origin: origin }, redirect: 'manual',
  body: new URLSearchParams({
    industry: '食品饮料', budget: '200', bus: '1', metro: '1', ...focusDates,
    target_centers: firstCenter, brief_objective: '不要引导到店，这次只提高品牌认知', brief_confirmed: '1',
  }),
}), '200');
assert.equal(goalDraft.status, 303);
const goalPath = goalDraft.headers.get('location');
const pendingGoal = await completedTask(goalPath);
assert.equal(pendingGoal.offer_goal_confirmation, true);
assert.match(pendingGoal.steps[2].notes.join(' '), /客户主目标待确认/u);
assert.doesNotMatch(pendingGoal.steps[2].notes.join(' '), /采用已确认的客户目标/u);
const pendingReport = await (await page(goalPath + '/review-copy?preview=1')).text();
assert.match(pendingReport, /待核草案保留两案/u);

const postGoal = (goal) => fetch(origin + goalPath + '/confirm-goal', {
  method: 'POST', headers: { Origin: origin }, redirect: 'manual', body: new URLSearchParams({ goal }),
});
assert.equal((await postGoal('通勤')).status, 422);
for (const [goal, heading] of [['拉新', '新客获取'], ['促销', '促销参与']]) {
  const confirmed = await postGoal(goal);
  assert.equal(confirmed.status, 303, await confirmed.text());
  const confirmedPath = confirmed.headers.get('location');
  const withGoal = await completedTask(confirmedPath);
  assert.equal(withGoal.offer_goal_confirmation, false);
  assert.deepEqual(withGoal.recovery_snapshots, pendingGoal.recovery_snapshots);
  assert.match(withGoal.steps[2].notes.join(' '), new RegExp('采用已确认的客户目标。以' + heading, 'u'));
  const changed = await fetch(origin + confirmedPath + '/rerun', {
    method: 'POST', headers: { Origin: origin }, redirect: 'manual',
    body: new URLSearchParams({ budget: '500', evaluation: '1' }),
  });
  assert.equal(changed.status, 303);
  const repriced = await completedTask(changed.headers.get('location'));
  assert.match(repriced.steps[2].notes.join(' '), new RegExp('沿用原版本已采用的策略。以' + heading, 'u'));
  assert.match(repriced.steps[3].notes.join(' '), /沿用原版本核算优先级/u);
}
const comparisonFixture = new FormData();
for (const [key, value] of Object.entries({ ...input, ...focusDates, observed_at: '2026-11-21T11:10' })) {
  comparisonFixture.set(key, value);
}
file(comparisonFixture, 'bus_stock', '线路名称,行车公司,线路等级,媒体名称,空车数\n10,测试运营方,A+,大三侧,1\n12,测试运营方,A+,大三侧,1');
file(comparisonFixture, 'metro_stock', '点位名称,站点等级,媒体形式,可售数量,面积\n演示站A号口,A+,灯箱,1,9.1');
const detailHeader = '明细分析\n中心点名称,媒体类型,媒体名称,覆盖半径,最近距离\n';
file(comparisonFixture, 'bus_coverage', detailHeader + [
  `${firstCenter},公交线路,10,1500,120`,
  `${firstCenter},公交线路,12,1500,800`,
  `${secondCenter},公交线路,12,1500,180`,
].join('\n'));
file(comparisonFixture, 'metro_coverage', detailHeader + `${firstCenter},地铁出入口,演示站A号口,1500,300`);
file(comparisonFixture, 'rates', '媒体类型,匹配对象,发布费,计价数量,计价周期,制作费\n公交车,A+,180万元,1,1月,0元\n地铁出入口,A+,20万元,1,1月,0元');
file(comparisonFixture, 'reach', [
  '媒体类型,资源编号或线路,统计月份,每日触达人次,计量单位,来源记录',
  '公交车身,10,2023-08,1000,人次/辆/天,隔离公交证据甲',
  '公交车身,12,2023-08,3000,人次/辆/天,隔离公交证据乙',
  '地铁出入口,演示站A号口,2023-08,600,人次/点位/天,隔离地铁证据',
].join('\n'));
const comparisonMap = {
  type: 'FeatureCollection',
  features: [
    ...['10','12'].map((resource, index) => ({ type: 'Feature',
      properties: { media: 'bus', resource, source_ref: '隔离公交路线' + resource },
      geometry: { type: 'LineString', coordinates: [[121.451 + index / 100, 31.221], [121.489, 31.237]] } })),
    { type: 'Feature', properties: { media: 'metro', resource: '演示站A号口', source_ref: '隔离出口' },
      geometry: { type: 'Point', coordinates: [121.46, 31.226] } },
  ],
};
comparisonFixture.set('route_geometry', new Blob([JSON.stringify(comparisonMap)], { type: 'application/geo+json' }), 'comparison.geojson');
const inserted = await fetch(origin + '/api/media-proposals/import', {
  method: 'POST', headers: { Origin: origin }, body: comparisonFixture, redirect: 'manual',
});
assert.equal(inserted.status, 303, await inserted.text());
assert.ok(new URL(inserted.headers.get('location'), origin).searchParams.get('batch'));
async function runComparisonBrief(goal, focus, customerNotes, requiredRoutes = '') {
  const result = await recoverImported(await fetch(origin + root + '/create', {
    method: 'POST', headers: { Origin: origin }, redirect: 'manual',
    body: new URLSearchParams({
      industry: '食品饮料', budget: '200', bus: '1', metro: '1', ...focusDates,
      target_centers: [firstCenter, secondCenter].join('\n'), core_circle: firstCenter,
      brief_campaign_goal: goal, brief_comparison_focus: focus, brief_objective: customerNotes,
      required_routes: requiredRoutes,
      brief_confirmed: '1',
      ops_source: '隔离施工条件', ops_hours: '22:00-05:00', ops_bus: '2', ops_metro: '1', ops_buffer: '1', ops_confirmed: '1',
      ops_windows: '2027-08-01 | 2027-07-25 至 2027-07-29 | 2027-09-01 至 2027-09-04',
    }),
  }), '200');
  assert.equal(result.status, 303, await result.text());
  return result.headers.get('location');
}
async function applyComparisonBrief(goal, focus, customerNotes) {
  const path = await runComparisonBrief(goal, focus, customerNotes);
  const status = await completedTask(path);
  assert.deepEqual(status.approval_readiness, { a: true, b: true });
  assert.equal(status.state, 'done');
  const html = await (await page(path + '/review-copy?preview=1')).text();
  const rows = financialRows(html).filter(([resource]) => ['10', '12'].includes(resource)).map(([resource]) => resource);
  return { html, rows, status, notes: status.steps[4].notes.join(' ') };
}
const visit = await applyComparisonBrief('到店', 'core_bus_days', '引导顾客在商圈门店参加试饮');
const awareness = await applyComparisonBrief('认知', '', '扩大两处商圈对新品包装的认知');
assert.match(visit.notes, /建议优先复核 B 案。核心圈 500 米内已核实的公交车日更多/u);
assert.match(awareness.notes, /建议优先复核 A 案。按有出处的单位触达和投放天数估算，其累计曝光人次更高/u);
assert.match(visit.html, /核心圈公交车日 · 31 车日/u);
assert.match(awareness.html, /111,600 人次/u);
assert.deepEqual(visit.rows, ['12', '10']);
assert.deepEqual(awareness.rows, ['12', '10']);
const preferredReach = await applyComparisonBrief('到店', 'estimated_exposure', '客户指定先看预计曝光，再分析到店机会');
assert.deepEqual(preferredReach.rows, ['12', '10']);
assert.match(preferredReach.notes, /建议优先复核 A 案。按有出处的单位触达和投放天数估算/u);
assert.match(preferredReach.notes, /采用客户首要比选指标 · 可核验预估人次/u);
assert.ok(preferredReach.html.includes('客户首要比选指标'));
for (const { html } of [visit, awareness, preferredReach]) {
  assert.match(html, /库存核对[^<]*2026-11-21[ T]11:10/u);
}
const substitutionPath = await runComparisonBrief('认知', '', '核对客户指定线路的同档期替换方式', '11路');
const substitution = await completedTask(substitutionPath);
assert.equal(substitution.state, 'review');
assert.equal(substitution.offer_review, false);
assert.equal(substitution.offer_route_replacement, true);
const substitutionReport = await (await page(substitutionPath + '/review-copy?preview=1')).text();
assert.match(substitutionReport, /必选线路缺口与同档期备选/u);
assert.match(substitutionReport, /11路<\/strong> · 红豆同档期快照无该线路记录/u);
assert.match(substitutionReport, /可售 1 辆 · 本段单辆 ¥1800000–1800000/u);
assert.match(substitutionReport, /覆盖 [^<]+ · 红豆[^<]+ · 刊例/u);
const replacementPath = substitutionPath + '/replacements';
const replacementPage = await (await page(replacementPath)).text();
assert.match(replacementPage, /name="original"[\s\S]+value="11路">11路/u);
assert.match(replacementPage, /name="alternative"[\s\S]+value="12">12/u);
assert.match(replacementPage, /新云覆盖[^<]+最近 180 米/u);
const submitReplacement = changes => fetch(origin + replacementPath, {
  method: 'POST', headers: { Origin: origin }, redirect: 'manual',
  body: new URLSearchParams({ original: '11路', alternative: '12路', reason: '客户确认相邻商圈覆盖', attested: '1', ...changes }),
});
assert.equal((await submitReplacement({ attested: '0' })).status, 422);
assert.equal((await submitReplacement({ alternative: '99路' })).status, 422);
assert.equal((await submitReplacement({ original: '10路' })).status, 422);
const replaced = await submitReplacement({});
assert.equal(replaced.status, 303, await replaced.text());
const replacedPath = replaced.headers.get('location');
const replacement = await completedTask(replacedPath);
assert.equal(replacement.state, 'done');
assert.equal(replacement.offer_route_replacement, false);
assert.match(replacement.steps[0].notes.join(' '), /必选线路 11路 → 12/u);
assert.match(replacement.steps[3].notes.join(' '), /完成两案/u);
const replacementReport = await (await page(replacedPath + '/review-copy?preview=1')).text();
assert.match(replacementReport, /入选依据 · 客户指定，并核对本段可售库存和刊例/u);
assert.doesNotMatch(replacementReport, /11路|必选线路缺口与同档期备选/u);
assert.equal(await (await page(substitutionPath + '/review-copy?preview=1')).text(), substitutionReport);
const replacementLog = await (await page('/audit?task=' + replacedPath.split('/').at(-1))).text();
assert.match(replacementLog, /客户确认替换必选线路/u);
assert.match(replacementLog, /客户确认相邻商圈覆盖/u);

const replacementPeriods = [
  { starts: '2027-07-04', ends: '2027-07-31', oneStage: '12' },
  { starts: '2027-08-01', ends: '2027-08-21', oneStage: '13' },
];
for (const { starts, ends, oneStage } of replacementPeriods) {
  const phase = new FormData();
  for (const [key, value] of Object.entries({ ...input, starts, ends })) phase.set(key, value);
  file(phase, 'bus_stock', `线路名称,行车公司,线路等级,媒体名称,空车数\n10,测试运营方,A+,大三侧,1\n${oneStage},测试运营方,A+,大三侧,1\n77,测试运营方,A+,大三侧,0`);
  file(phase, 'bus_coverage', `线路名称,目标圈,半径\n10,目标商圈,300\n${oneStage},目标商圈,300\n77,目标商圈,300`);
  file(phase, 'metro_stock', '点位名称,站点等级,媒体形式,可售数量,面积\n演示站A号口,A+,灯箱,1,9.1');
  file(phase, 'metro_coverage', '点位名称,目标圈,半径\n演示站A号口,目标商圈,300');
  file(phase, 'rates', '媒体类型,匹配对象,发布费,计价数量,计价周期,制作费\n公交车,A+,60万元,1,1月,0元\n地铁出入口,A+,40万元,1,1月,0元');
  const imported = await fetch(origin + '/api/media-proposals/import', {
    method: 'POST', headers: { Origin: origin }, body: phase, redirect: 'manual',
  });
  assert.equal(imported.status, 303, await imported.text());
}
const periodDraft = await recoverImported(await fetch(origin + root + '/create', {
  method: 'POST', headers: { Origin: origin }, redirect: 'manual',
  body: new URLSearchParams({
    industry: '食品饮料', budget: '200', bus: '1', metro: '1',
    starts: replacementPeriods[0].starts, ends: replacementPeriods[1].ends,
    required_routes: '11路', brief_campaign_goal: '认知', brief_confirmed: '1',
    scheduling_factors: '2027-07-01 | 3 | 主投验收\n2027-08-01 | 1 | 补投验收',
  }),
}), '200');
assert.equal(periodDraft.status, 303, await periodDraft.text());
const periodDraftPath = periodDraft.headers.get('location');
const periodDraftStatus = await completedTask(periodDraftPath);
assert.deepEqual(periodDraftStatus.recovery_snapshots.map(({ starts, ends }) => ({ starts, ends })),
  replacementPeriods.map(({ starts, ends }) => ({ starts, ends })));
const periodReplacement = periodDraftPath + '/replacements';
const periodPage = await (await page(periodReplacement)).text();
const offered = periodPage.match(/<select name="alternative"[\s\S]*?<\/select>/u)?.[0];
assert.match(offered, /value="10">10/u);
assert.doesNotMatch(offered, /value="(?:12|13|77)"/u);
const postPeriod = alternative => fetch(origin + periodReplacement, {
  method: 'POST', headers: { Origin: origin }, redirect: 'manual',
  body: new URLSearchParams({ original: '11路', alternative, reason: '客户确认跨段采用同一路线', attested: '1' }),
});
assert.equal((await postPeriod('12')).status, 422);
const acceptedPeriods = await postPeriod('10');
assert.equal(acceptedPeriods.status, 303, await acceptedPeriods.text());
const acceptedPeriodPath = acceptedPeriods.headers.get('location');
const acceptedPeriodStatus = await completedTask(acceptedPeriodPath);
assert.equal(acceptedPeriodStatus.state, 'review');
assert.equal(acceptedPeriodStatus.offer_route_replacement, false);
assert.deepEqual(acceptedPeriodStatus.recovery_snapshots, periodDraftStatus.recovery_snapshots);
assert.match(acceptedPeriodStatus.steps[0].notes.join(' '), /必选线路 11路 → 10，2 段/u);
const acceptedPeriodReport = await (await page(acceptedPeriodPath + '/review-copy?preview=1')).text();
assert.doesNotMatch(acceptedPeriodReport, /必选线路缺口与同档期备选/u);
assert.match(acceptedPeriodReport, /投放条件说明/u);

const reviseBase = await runComparisonBrief('认知', '', '扩大新品包装在目标商圈的认知');
const beforeRevision = await completedTask(reviseBase);
const previousRevisionReport = await (await page(reviseBase + '/review-copy?preview=1')).text();
assert.match(previousRevisionReport, /品牌类型及规模待确认|详细目标待补充|目标人群待补充/u);
const campaignInput = {
  customer_profile: '上海某区域食品饮料连锁，示例覆盖 12 家门店',
  objective: '提高暑期新品外包装在两处重点商圈的认知',
  goal: '认知',
  audience: '在上海商圈工作和通勤的人群',
  region: '静安与徐汇的两个门店目标圈',
  monitoring: '按门店统计包装认知问卷，核对上下刊照片并提交结案报告',
};
const reviseCampaign = changes => fetch(origin + reviseBase + '/campaign', {
  method: 'POST', headers: { Origin: origin }, redirect: 'manual',
  body: new URLSearchParams({ ...campaignInput, ...changes }),
});
assert.equal((await reviseCampaign({})).status, 422);
assert.equal((await reviseCampaign({ attested: '1', monitoring: '' })).status, 422);
const revised = await reviseCampaign({ attested: '1' });
assert.equal(revised.status, 303, await revised.text());
const revisedPath = revised.headers.get('location');
const revision = await completedTask(revisedPath);
assert.equal(revision.state, 'done');
assert.match(revision.generation_time, /^\d+\.\d{2} 秒$/u);
assert.deepEqual(revision.approval_readiness, beforeRevision.approval_readiness);
const revisedReport = await (await page(revisedPath + '/review-copy?preview=1')).text();
assert.deepEqual(financialRows(revisedReport), financialRows(previousRevisionReport));
const campaignSheet = await page(revisedPath + '/campaign');
assert.match(campaignSheet.headers.get('content-disposition'), /cityline-campaign\.html/u);
const campaignHtml = await campaignSheet.text();
for (const text of Object.values(campaignInput)) {
  assert.ok(campaignHtml.includes(text) && revisedReport.includes(text), text);
}
assert.doesNotMatch(campaignHtml + revisedReport, /假设书待补/u);
assert.match(revisedReport, /href="#proposal-variants">查看目标圈、线路与出口依据/u);
assert.match(revisedReport, /href="#calendar">查阅档期库存与报价/u);
assert.match(revisedReport, /href="#measurement">查看监测与验收约定/u);
assert.match(revisedReport, /发布费 200\.00–200\.00 万元，制作费 0\.00–0\.00 万元/u);
assert.match(revisedReport, /入选依据 · 命中目标圈/u);
assert.equal(await (await page(reviseBase + '/review-copy?preview=1')).text(), previousRevisionReport);
assert.equal((await reviseCampaign({ attested:'1', goal:'到店', trial_stores:trialInput.trial_stores })).status,422);
assert.equal((await reviseCampaign({ attested:'1', ...trialInput })).status,422);
const revisedGoal = await reviseCampaign({ attested: '1', goal: '到店', monitoring:'', ...trialInput });
assert.equal(revisedGoal.status, 303, await revisedGoal.text());
const revisedGoalPath = revisedGoal.headers.get('location');
const revisedGoalStatus = await completedTask(revisedGoalPath);
assert.match(revisedGoalStatus.steps[2].notes.join(' '), /采用已确认的客户目标。以到访引导为重点/u);
assert.deepEqual(revisedGoalStatus.approval_readiness, beforeRevision.approval_readiness);
const trialSheet = await (await page(revisedGoalPath + '/campaign')).text();
assert.doesNotMatch(trialSheet,/假设书待补/u);
assert.match(trialSheet,/到店试饮有效扫码量。投放后按匿名事件去重/u);
const schedulePath = revisedGoalPath + '/schedule';
const schedulePage = await (await page(schedulePath)).text();
assert.match(schedulePage, /原采购档期/u);
assert.ok(schedulePage.includes(trialInput.trial_window));
const oldScheduleReport = await (await page(revisedGoalPath + '/review-copy?preview=1')).text();
const movedDates = { starts:'2027-09-07', ends:'2027-09-20' };
const moveSchedule = (overrides = {}) => fetch(origin + schedulePath, {
  method:'POST', headers:{ Origin:origin }, redirect:'manual', body:new URLSearchParams({
    ...movedDates, reason:'客户调整试饮活动窗口', trial_window:'九月实际投放段的营业期间',
    confirmed:'1', local_snapshot:'1', ...overrides,
  }),
});
assert.equal((await moveSchedule({ confirmed:'0' })).status, 422);
assert.equal((await moveSchedule({ ends:'2027-09-06' })).status, 422);
assert.equal((await moveSchedule({ trial_window:'' })).status, 422);
assert.equal((await moveSchedule(focusDates)).status, 409);
assert.equal((await moveSchedule()).status, 409);
const movedAwaitingLogin = await moveSchedule({ local_snapshot:'0' });
assert.equal(movedAwaitingLogin.status, 303);
const movedPreflight = await completedTask(movedAwaitingLogin.headers.get('location'));
assert.equal(movedPreflight.login.phase,'error_preflight');
assert.deepEqual(movedPreflight.recovery_snapshots,[]);
const movedFixture = new FormData();
for (const [field,value] of comparisonFixture) movedFixture.set(field,value);
for (const [field,value] of Object.entries(movedDates)) movedFixture.set(field,value);
const movedImported = await fetch(origin + '/api/media-proposals/import', {
  method:'POST', headers:{ Origin:origin }, redirect:'manual', body:movedFixture,
});
assert.equal(movedImported.status,303);
const movedBatch = new URL(movedImported.headers.get('location'),origin).searchParams.get('batch');
const moved = await moveSchedule();
assert.equal(moved.status,303,await moved.text());
const movedPath = moved.headers.get('location');
const movedStatus = await completedTask(movedPath);
assert.equal(movedStatus.state,'review');
assert.equal(movedStatus.offer_operations,true);
assert.equal(movedStatus.offer_review,false);
assert.deepEqual(movedStatus.recovery_snapshots,[{id:movedBatch,...movedDates}]);
const movedReport = await (await page(movedPath + '/review-copy?preview=1')).text();
assert.match(movedReport,/2027-08-01 至 2027-08-31 → 2027-09-07 至 2027-09-20/u);
assert.match(movedReport,/新的上下刊施工窗口待运营确认/u);
assert.match(movedReport,/九月实际投放段的营业期间/u);
assert.ok(movedReport.includes(trialInput.trial_baseline));
assert.equal(await (await page(revisedGoalPath + '/review-copy?preview=1')).text(),oldScheduleReport);
assert.match(await (await page('/audit?task='+movedPath.split('/').at(-1))).text(),/调整投放档期/u);

comparisonFixture.set('observed_at', '2026-11-21T11:20');
file(comparisonFixture, 'bus_coverage', detailHeader + [
  `${firstCenter},公交线路,10,1500,120`,
  `${secondCenter},公交线路,12,1500,180`,
].join('\n'));
const incompleteMetricImport = await fetch(origin + '/api/media-proposals/import', {
  method: 'POST', headers: { Origin: origin }, body: comparisonFixture, redirect: 'manual',
});
assert.equal(incompleteMetricImport.status, 303, await incompleteMetricImport.text());
const pendingMetricPath = await runComparisonBrief('到店', 'core_bus_days', '两案必须先核对核心圈公交车日');
const pendingMetric = await completedTask(pendingMetricPath);
assert.deepEqual(pendingMetric.approval_readiness, { a: true, b: true });
assert.equal(pendingMetric.state, 'review');
assert.equal(pendingMetric.offer_review, false);
const pendingMetricReport = await (await page(pendingMetricPath + '/review-copy?preview=1')).text();
assert.doesNotMatch(pendingMetricReport, workflowCopy);
assert.match(pendingMetricReport, /客户指定的首要指标尚无法比较/u);
assert.doesNotMatch(pendingMetricReport, /建议优先复核 [AB] 案/u);
const pendingMetricApproval = await fetch(origin + pendingMetricPath + '/review', {
  method: 'POST', headers: { Origin: origin }, redirect: 'manual',
  body: new URLSearchParams({ decision: 'approved', variant: 'A', note: '隔离验收' }),
});
assert.equal(pendingMetricApproval.status, 409);

const monthlyDates = { starts: '2027-11-03', ends: '2027-11-24' };
const monthlyFixture = new FormData();
for (const [key, value] of Object.entries({ ...input, ...monthlyDates, observed_at: '2026-11-21T11:30' })) {
  monthlyFixture.set(key, value);
}
file(monthlyFixture, 'bus_stock', '线路名称,行车公司,线路等级,媒体名称,空车数\n10,测试运营方,A+,大三侧,1');
file(monthlyFixture, 'bus_coverage', '线路名称,目标圈,半径\n10,目标商圈,300');
file(monthlyFixture, 'metro_stock', '点位名称,站点等级,媒体形式,可售数量,面积\n演示站A号口,A+,灯箱,1,9.1');
file(monthlyFixture, 'metro_coverage', '点位名称,目标圈,半径\n演示站A号口,地铁目标圈,250');
file(monthlyFixture, 'rates', '媒体类型,匹配对象,发布费,计价数量,计价周期,制作费\n公交车,A+,约3–5万,1,1月,约5–10千\n地铁出入口,A+,约15–25万,1,1月,100元/㎡');
monthlyFixture.set('route_geometry', new Blob([JSON.stringify({
  ...comparisonMap,
  features: comparisonMap.features.filter(row => row.properties.resource !== '12'),
})], { type: 'application/geo+json' }), 'monthly.geojson');
file(monthlyFixture, 'reach', [
  '媒体类型,资源编号或线路,统计月份,每日触达人次,计量单位,来源记录,估算基准',
  '公交车身,10,2023-05,700,人次/辆/天,公交五月基准,新云2023-05基准',
  '公交车身,10,2023-11,1500,人次/辆/天,公交十一月单位触达,',
  '地铁出入口,演示站A号口,2023-05,400,人次/点位/天,地铁五月基准,新云2023-05基准',
  '地铁出入口,演示站A号口,2023-11,99,平台标签日均人次,地铁十一月区域人流,',
  '地铁出入口,演示站A号口,2023-10,5000,平台标签日均人次,非本档期区域人流,',
].join('\n'));
const monthlyImported = await fetch(origin + '/api/media-proposals/import', {
  method: 'POST', headers: { Origin: origin }, body: monthlyFixture, redirect: 'manual',
});
assert.equal(monthlyImported.status, 303, await monthlyImported.text());
const monthlyTask = await run({ industry: 'food', media: 'both', budget: '200', ...monthlyDates });
assert.equal(monthlyTask.status, 303, await monthlyTask.text());
const monthlyPath = await confirmTaskGoal(monthlyTask.headers.get('location'));
assert.equal((await completedTask(monthlyPath)).state, 'done');
const monthlyReport = await (await page(monthlyPath + '/review-copy?preview=1')).text();
assert.match(monthlyReport, /2023 年 11 月 · 1,500 人次\/辆\/天/u);
assert.match(monthlyReport, /2023 年 5 月 · 400 人次\/点位\/天<small>历史基准用于 2027 年 11 月估算/u);
assert.match(monthlyReport, /41,800 人次/u);
assert.match(monthlyReport, /报告数据一览[\s\S]*两案关键数据[\s\S]*41,800/u);
const monthlyExposure = monthlyReport.match(/<dl[^>]*aria-label="本档期曝光估算"[^>]*>([\s\S]*?)<\/dl>/u)?.[1] ?? '';
assert.match(monthlyExposure, /公交车身<\/dt><dd><strong>33,000 人次/u);
assert.match(monthlyExposure, /地铁出入口<\/dt><dd><strong>8,800 人次<\/strong> · 完整估算 · 含 5 月基准/u);
assert.match(monthlyExposure, /组合曝光<\/dt><dd><strong>41,800 人次/u);
assert.match(monthlyReport, /1 个资源月份采用 2023 年 5 月历史基准/u);
assert.match(monthlyReport, /99 人次\/天<small>2023 年 11 月 · 平台标签口径/u);
assert.doesNotMatch(monthlyReport, /非本档期区域人流|公交五月基准/u);

const archiveDates = { starts:'2027-08-20', ends:'2027-09-19' };
const archived = await importData({ ...input, ...archiveDates }, false, 'data');
assert.equal(archived.status, 303);
const archiveUrl = archived.headers.get('location');
assert.match(archiveUrl, /^\/media-data\?batch=[a-f0-9-]{36}$/u);
const archiveId = new URL(archiveUrl, origin).searchParams.get('batch');
const archivePage = await (await page(archiveUrl)).text();
const fromSnapshot = '/media-data/snapshots/' + archiveId + '/proposals';
assert.ok(archivePage.includes(`action="${fromSnapshot}"`));
assert.match(archivePage, /返回本地快照/u);
assert.match(archivePage, /用此快照生成提案/u);
// A newer archive for the same interval must not replace an explicitly selected version.
assert.equal((await importData({ ...input, ...archiveDates, observed_at:'2026-11-21T11:59' }, false)).status, 303);
const snapshotPost = (path, values = {}) => fetch(origin + path, {
  method:'POST', headers:{ Origin:origin }, redirect:'manual', body:new URLSearchParams(values),
});
assert.equal((await snapshotPost('/media-data/snapshots/00000000-0000-4000-8000-000000000000/proposals')).status, 404);
const replay = await snapshotPost(fromSnapshot);
assert.equal(replay.status, 303, await replay.text());
const replayPath = replay.headers.get('location');
const archiveTask = await completedTask(replayPath);
assert.equal(archiveTask.state, 'review');
assert.equal(archiveTask.platform_interrupted, false);
assert.equal(archiveTask.offer_operations, true);
assert.equal(archiveTask.offer_goal_confirmation, true);
assert.equal(archiveTask.offer_review, false);
assert.deepEqual(archiveTask.recovery_snapshots, [{ id:archiveId, ...archiveDates }]);
const archiveAudit = await (await page('/audit?task=' + replayPath.split('/').at(-1))).text();
assert.match(archiveAudit, /使用快照生成/u);
assert.ok(archiveAudit.includes(archiveId));
const archiveReport = await (await page(replayPath + '/review-copy?preview=1')).text();
assert.match(archiveReport, /按已导入快照的完整档期核算/u);
assert.match(archiveReport, /2027-08-20 至 2027-09-19/u);
const archiveOps = await snapshotPost(replayPath + '/operations', {
  ops_source:'隔离窗口记录', ops_hours:'22:00-05:00', ops_bus:'2', ops_metro:'1',
  ops_buffer:'1', ops_confirmed:'1',
  ops_windows:'2027-08-20 | 2027-08-15 至 2027-08-18 | 2027-09-20 至 2027-09-23',
});
assert.equal(archiveOps.status, 303, await archiveOps.text());
const archiveRevised = await completedTask(archiveOps.headers.get('location'));
assert.deepEqual(archiveRevised.recovery_snapshots, [{ id:archiveId, ...archiveDates }]);
assert.equal(archiveRevised.offer_review,false);
console.log('媒体提案验证通过。预算复算、库存核验、按 Brief 选线及 A／B 比选、HTML 交付均已检查。');
