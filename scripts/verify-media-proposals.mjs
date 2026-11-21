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
assert.doesNotMatch(home, /name="bus_stock"|费用区间/u);
assert.doesNotMatch(home, /name="source"/u);
assert.equal((await page(root)).headers.get('referrer-policy'), 'same-origin');
const data = await (await page('/media-data')).text();
assert.match(data, /手动补录赛用数据/u);
assert.match(data, /可用数据快照/u);
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
assert.match(offline, /<details class="media-item">/u);
assert.match(offline, /<body class="report-page">/u);
assert.match(offline, /class="report-schedule"/u);
for (const heading of ['公司简介', '目标圈与媒介组合', '发布推荐', '采购清单与刊例', '增值权益与交付']) {
  assert.ok(offline.includes(heading), heading);
}
assert.equal((offline.match(/预算目标量级<\/dt><dd>约 200 万元<\/dd>/gu) ?? []).length, 2);
assert.match(offline, /当前未确认赠送媒体或代理优惠/u);
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
    assert.match(html, /导出当前方案 HTML/u, scenario);
  }
}
await page(root + '?budget=300', 400);
await page(root + '/export?demo=1&starts=2027-07-01&ends=2027-07-31', 409);

const sourceDate = { starts: '2027-07-01', ends: '2027-07-31' };
function file(form, name, text) {
  form.set(name, new Blob([text], { type: 'text/csv' }), name + '.csv');
}

async function importData(fields, addEvidence = true) {
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
  return fetch(origin + '/api/media-proposals/import', {
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
  assert.match(initial.steps[1].notes.join(' '), /本机尚未设置赛题刊例路径/u);
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
const taskPath = executed.headers.get('location');
assert.match(taskPath ?? '', /^\/media-proposals\/tasks\/[a-f0-9-]{36}$/u);
async function completedTask(path) {
  const deadline = Date.now() + 125000;
  while (Date.now() < deadline) {
    const status = await (await page('/api' + path)).json();
    if (status.state !== 'active') return status;
    await new Promise((resolve) => setTimeout(resolve, 350));
  }
  assert.fail('提案任务未在时限内结束');
}
const done = await completedTask(taskPath);
assert.equal(done.state, 'done');
assert.equal(done.steps.length, 5);
assert.match(done.steps[1].notes.join(' '), /已锁定赛用数据快照/u);
assert.match(done.steps[4].notes.join(' '), /HTML 已渲染并保存/u);
const taskResult = await (await page(taskPath)).text();
assert.match(taskResult, /预览独立 HTML/u);
assert.doesNotMatch(taskResult, /type="file"/u);
const taskReport = await page(taskPath + '/report?theme=dark');
assert.match(taskReport.headers.get('content-disposition') ?? '', /attachment;/u);
const taskHtml = await taskReport.text();
assert.match(taskHtml, /data-theme="light"/u);
assert.equal((taskHtml.match(/name="report-view"/gu) ?? []).length, 3);
assert.match(taskHtml, /提案思路/u);
assert.match(taskHtml, /¥221000–371000/u);
assert.match(taskHtml, /最低高位报价合计 <strong>31\.10 万元<\/strong>/u);
assert.match(taskHtml, /地铁出入口 · 演示站A号口 · 灯箱 · ¥251000/u);
assert.equal((taskHtml.match(/<article class="report-variant"/gu) ?? []).length, 2);
assert.equal((taskHtml.match(/<polyline class="route-map-path"/gu) ?? []).length, 2);
assert.match(taskHtml, /日均 2,600 人次/u);
assert.match(taskHtml, /80,600 人次/u);
const julyFormal = await recoverImported(await fetch(origin + root + '/create', {
  method: 'POST', headers: { Origin: origin }, redirect: 'manual',
  body: new URLSearchParams({ industry: '食品饮料', budget: '200', bus: '1', metro: '1', ...sourceDate }),
}), '200');
assert.equal(julyFormal.status, 303);
const julyFormalPath = julyFormal.headers.get('location');
const awaitingOps = await completedTask(julyFormalPath);
assert.equal(awaitingOps.state, 'review');
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
const evidenceDraftPath = evidenceDraft.headers.get('location');
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
const draftReport = await (await page(evidenceDraftPath + '/report?preview=1')).text();
assert.match(draftReport, /待补新云触达数据/u);
assert.match(await (await page(evidenceDraftPath)).text(), /查看补证清单/u);
const checklist = await (await page(evidenceDraftPath + '/evidence')).text();
assert.match(checklist, /2027-07 缺少 人次\/辆\/天/u);
assert.match(checklist, /A／B/u);
assert.match(checklist, /补充本档期证据/u);
assert.doesNotMatch(checklist, /人次\/点位\/天/u);
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
file(fixtureAugment, 'reach', '媒体类型,资源编号或线路,统计月份,每日触达人次,计量单位,来源记录\n公交车身,99路,2023-07,1234,人次/辆/天,测试补录原行');
fixtureAugment.set('route_geometry', new Blob([JSON.stringify({
  type: 'FeatureCollection', features: [{ type: 'Feature',
    properties: { media: 'bus', resource: '99路', source_ref: '隔离测试坐标' },
    geometry: { type: 'LineString', coordinates: [[121.44, 31.21], [121.48, 31.25]] },
  }],
})], { type: 'application/geo+json' }), 'test-recovered-route.geojson');
const busSnapshot = new URL(moreRecent.headers.get('location'), origin).searchParams.get('batch');
const evidencePath = '/api/media-proposals/snapshots/' + busSnapshot + '/evidence';
const evidenceTaskId = evidenceDraftPath.split('/').at(-1);
assert.deepEqual(evidenceState.recovery_snapshots, [{ id: busSnapshot, starts: '2027-07-01', ends: '2027-07-31' }]);
assert.equal(evidenceState.offer_same_inventory, true);
const pinnedUrl = '/media-data?batch=' + busSnapshot + '&task=' + evidenceTaskId;
const evidenceDetail = await (await page(evidenceDraftPath)).text();
assert.ok(evidenceDetail.includes(pinnedUrl.replace('&', '&amp;')));
const pinnedPage = await (await page(pinnedUrl)).text();
assert.equal((pinnedPage.match(/class="snapshot-row"/gu) ?? []).length, 1);
assert.match(pinnedPage, /<details class="import-options" open>/u);
assert.ok(pinnedPage.includes('action="' + evidencePath + '?task=' + evidenceTaskId + '"'));
const augmented = await fetch(origin + evidencePath + '?task=' + evidenceTaskId, {
  method: 'POST', headers: { Origin: origin }, body: fixtureAugment, redirect: 'manual',
});
assert.equal(augmented.status, 303, await augmented.text());
const augmentedUrl = new URL(augmented.headers.get('location'), origin);
assert.equal(augmentedUrl.searchParams.get('task'), evidenceTaskId);
assert.notEqual(augmentedUrl.searchParams.get('batch'), busSnapshot);
const augmentedPage = await (await page(augmentedUrl.pathname + augmentedUrl.search)).text();
assert.ok(augmentedPage.includes(`href="${evidenceDraftPath}">返回原提案任务</a>`));
const recovered = await refreshEvidence();
assert.equal(recovered.status, 303, await recovered.text());
const recoveredPath = recovered.headers.get('location');
assert.notEqual(recoveredPath, evidenceDraftPath);
const recoveredState = await completedTask(recoveredPath);
assert.equal(recoveredState.state, 'done');
assert.match(recoveredState.steps[0].notes.join(' '), /已沿用原 A／B 采购清单和报价/u);
const recoveredReport = await (await page(recoveredPath + '/report?preview=1')).text();
assert.doesNotMatch(await (await page(recoveredPath + '/evidence')).text(), /2027-07 缺少 人次\/辆\/天/u);
assert.match(recoveredReport, /38,254 人次/u);
const financialRows = (html) => [...html.matchAll(
  /<tr>\s*<th scope="row">([^<]+)<small>[^<]*<\/small>[\s\S]*?<\/th>\s*((?:<td>[^<]*<\/td>){5})/gu,
)].map(([, resource, financials]) => [resource, financials]);
assert.equal(financialRows(draftReport).length, 2);
assert.deepEqual(financialRows(recoveredReport), financialRows(draftReport));
assert.equal(await (await page(evidenceDraftPath + '/report?preview=1')).text(), draftReport);
const postReview = (decision, note) => fetch(origin + recoveredPath + '/review', {
  method: 'POST', headers: { Origin: origin }, redirect: 'manual',
  body: new URLSearchParams({ decision, note }),
});
assert.equal((await postReview('returned', '')).status, 422);
const concurrent = await Promise.all([postReview('approved', '核验恢复完成'), postReview('returned', '并发复核')]);
assert.deepEqual(concurrent.map(r => r.status).sort(), [303, 409]);
const recoveredPage = await (await page(recoveredPath)).text();
assert.ok(recoveredPage.includes(evidenceDraftPath));
assert.match(recoveredPage, /审核意见/u);
assert.doesNotMatch(recoveredPage, /name="decision"/u);
const scopedAuditUrl = '/audit?task=' + recoveredPath.split('/').at(-1);
assert.ok(recoveredPage.includes(`href="${scopedAuditUrl}">查看任务审计</a>`));
const scopedAudit = await (await page(scopedAuditUrl)).text();
assert.match(scopedAudit, /人工审核/u);
assert.ok(!scopedAudit.includes(`href="${evidenceDraftPath}"`));
const recoveryAudit = await (await page('/audit')).text();
assert.match(recoveryAudit, /重新生成/u);
assert.match(recoveryAudit, /补证复核/u);
const bothStill = await(await page(location.replace(root + '?', root + '/export?'))).text();
assert.match(bothStill, /¥221000–371000/u);
const busCatalog = await(await page('/media-data?starts=2027-07-01&ends=2027-07-31')).text();
const newestBus = busCatalog.match(/media=bus&amp;starts=2027-07-01&amp;ends=2027-07-31&amp;batch=([a-f0-9-]{36})/u)?.[1];
assert.ok(newestBus && newestBus !== busSnapshot);
const freshestBus = await(await page(root + '/export?industry=food&starts=2027-07-01&ends=2027-07-31&media=bus&batch=' + newestBus)).text();
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
const retainedReport = await (await page(retainedPath + '/report?preview=1')).text();
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
assert.match(await (await page(replannedPath + '/report?preview=1')).text(), /<td>2 \/ 2<\/td>/u);
const recalculated = await recoverImported(await fetch(origin + taskPath + '/rerun', {
  method: 'POST', headers: { Origin: origin }, redirect: 'manual',
  body: new URLSearchParams({ budget: '150' }), signal: AbortSignal.timeout(10000),
}), '150');
assert.equal(recalculated.status, 303);
const recalculatedPath = recalculated.headers.get('location');
const rebudgeted = await completedTask(recalculatedPath);
assert.equal(rebudgeted.state, 'done');
assert.match(rebudgeted.summary, /150 万元/u);
assert.match(await (await page(recalculatedPath + '/report')).text(), /与 200 万预算方案比较/u);
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
    budget: '200', bus: '1', metro: '1', scheduling_factors: '2027-10-01 | 3 | 测试活动档期',
  }),
}), '200');
assert.equal(rolling.status, 303, await rolling.text());
const rollingPath = rolling.headers.get('location');
assert.equal((await completedTask(rollingPath)).state, 'review');
const rollingHtml = await (await page(rollingPath + '/report')).text();
assert.match(rollingHtml, /2027-01-01 至 2027-12-31/u);
assert.match(rollingHtml, /2027-10-01 至 2027-10-31<\/strong> · 31 天 · 待核对/u);
assert.match(rollingHtml, /测试活动档期 · 调配权重 3/u);
assert.match(rollingHtml, /机动 · 无采买/u);
assert.match(rollingHtml, /预算已预留/u);
assert.match(rollingHtml, /Campaign 假设书/u);
const catalog = await (await page('/media-data')).text();
assert.match(catalog, /核对快照/u);
assert.ok(catalog.includes(new URL(origin + location).searchParams.get('batch')));
const unavailable = await run({industry: 'food', month: '2027-08', media: 'both', budget: '200'});
assert.equal(unavailable.status, 303);
const blocked = await completedTask(unavailable.headers.get('location'));
assert.equal(blocked.state, 'review');
assert.match(blocked.steps[1].notes.join(' '), /本机尚未设置赛题刊例路径/u);
assert.equal((await fetch(origin + unavailable.headers.get('location') + '/report')).status, 409);

const brief = new FormData();
const retiredBrand = String.fromCodePoint(0x661f, 0x5df4, 0x514b);
brief.set('text', `主题：${retiredBrand}门店试饮\n行业：食品饮料\n投放预算：200万元\n媒体形式：公交车身及地铁出入口\n投放周期：2027年7月\n指定线路：10路、11路\n投放目标：门店试饮活动\n脱敏客户规模：上海区域连锁，已核对12处门店\n主目标：到店\n目标人群：工作日午间通勤者`);
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
assert.equal(extracted.brief.audience, '工作日午间通勤者');

async function createWithRoutes(routes) {
  const response = await recoverImported(await fetch(origin + root + '/create', {
    method: 'POST', headers: { Origin: origin }, redirect: 'manual',
    body: new URLSearchParams({
      industry: '食品饮料', starts: '2027-01-01', ends: '2027-12-31',
      budget: '200', bus: '1', metro: '1', required_routes: routes,
    }),
  }), '200');
  assert.equal(response.status, 303);
  const path = response.headers.get('location');
  return { path, job: await completedTask(path) };
}
const selectedRoute = await createWithRoutes('10路');
assert.equal(selectedRoute.job.state, 'review');
assert.match(selectedRoute.job.steps[4].notes.join(' '), /由本地规则执行报告核验 Agent/u);
const selectedReport = await (await page(selectedRoute.path + '/report')).text();
assert.match(selectedReport, /10路 · 已核对档期可售余量与刊例/u);
assert.match(selectedReport, /目标人群/u);
assert.equal((selectedReport.match(/预算目标量级<\/dt><dd>约 200 万元<\/dd>/gu) ?? []).length, 2);
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
const evaluationHtml = await (await page(evaluationPath + '/report')).text();
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
const followupHtml = await (await page(followupPath + '/report')).text();
assert.match(followupHtml, /预算变化 · 100 万元 → 500 万元/u);
assert.doesNotMatch(followupHtml, /预算变化 · 200 万元 → 500 万元/u);
assert.deepEqual([...followupHtml.matchAll(/data-budget-strategy="([AB])"/gu)].map(match => match[1]), ['A', 'B']);

const draft = await createWithRoutes('11路、12路');
assert.equal(draft.job.state, 'review');
assert.equal(draft.job.has_alternative, true);
const draftResponse = await page(draft.path + '/report?preview=1');
const draftAnnual = await draftResponse.text();
assert.match(draftAnnual, /备选草案 · 待确认/u);
assert.match(draftAnnual, /11路 · 红豆记录显示该档期无可售余量/u);
assert.match(draftAnnual, /12路 · 红豆同档期快照无该线路记录/u);
assert.match(draftAnnual, /整案最低报价待核对/u);
assert.doesNotMatch(draftAnnual, /最低高位报价合计 <strong>/u);
const approveDraft = await fetch(origin + draft.path + '/review', {
  method: 'POST', headers: { Origin: origin }, body: new URLSearchParams({ decision: 'approved', note: '' }),
});
assert.equal(approveDraft.status, 409);
const exported = await page(location.replace(root + '?', root + '/export?'));
assert.match(await exported.text(), /上刊准备/u);
const wrong = await (await page(root + '?industry=food&starts=2027-08-01&ends=2027-08-31')).text();
assert.match(wrong, /该档期没有可查看的取数记录/u);
assert.doesNotMatch(wrong, /¥221000–371000/u);
const mayImport = await importData({ ...input, starts: '2027-05-01', ends: '2027-05-31' }, false);
assert.equal(mayImport.status, 303, await mayImport.text());
assert.equal((await importData({ ...input, starts: '2027-10-01', ends: '2027-10-31' })).status, 303);
const annualDraft = await createWithRoutes('10路');
assert.equal(annualDraft.job.state, 'review');
assert.deepEqual(annualDraft.job.recovery_snapshots.map(({ starts }) => starts), ['2027-05-01', '2027-07-01', '2027-10-01']);
const annualDraftReport = await (await page(annualDraft.path + '/report?preview=1')).text();
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
const opsHtml = await (await page(opsPath + '/report?preview=1')).text();
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
const annualRecoveryReport = await (await page(annualRecoveryPath + '/report?preview=1')).text();
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
const createFocused = (center, confirmed) => fetch(origin + root + '/create', {
  method: 'POST', headers: { Origin: origin }, redirect: 'manual',
  body: new URLSearchParams({
    industry: '食品饮料', budget: '200', bus: '1', metro: '1', ...focusDates, target_centers: center,
    brief_customer_profile: extracted.brief.customer_profile, brief_objective: '验证午间试饮需求',
    brief_campaign_goal: '到店', brief_audience: extracted.brief.audience,
    brief_region: '门店午休动线', brief_monitoring: '各门店按周统计试饮扫码', brief_confirmed: String(confirmed),
  }),
});
assert.equal((await createFocused(firstCenter, 0)).status, 422);
assert.equal((await createFocused('无效地标', 1)).status, 422);
for (const [center, batch, route] of [[firstCenter, twoCenters, '10'], [secondCenter, newerSecondCenter, '12']]) {
  const started = await recoverImported(await createFocused(center, 1), '200');
  assert.equal(started.status, 303);
  const scopedPath = started.headers.get('location');
  const scoped = await completedTask(scopedPath);
  assert.deepEqual(scoped.recovery_snapshots, [{ id: batch, ...focusDates }]);
  const html = await (await page(scopedPath + '/report?preview=1')).text();
  const sheetResponse = await page(scopedPath + '/campaign');
  assert.match(sheetResponse.headers.get('content-disposition'), /cityline-campaign\.html/u);
  const sheet = await sheetResponse.text();
  for (const value of [center, extracted.brief.customer_profile, extracted.brief.audience, '门店午休动线', '各门店按周统计试饮扫码']) {
    assert.ok(html.includes(value) && sheet.includes(value), value);
  }
  assert.equal(financialRows(html).filter(([resource]) => ['10', '12'].includes(resource)).length, 2);
  assert.ok(financialRows(html).filter(([resource]) => ['10', '12'].includes(resource)).every(([resource]) => resource === route));
}
console.log('媒体提案验证通过。合成重算、预算档位、日期约束、库存拦截、刊例核算和 HTML 导出均已检查。');
