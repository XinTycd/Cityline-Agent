import assert from 'node:assert/strict';
import { mkdir, readFile, writeFile } from 'node:fs/promises';
import { basename, join } from 'node:path';

const port = Number(process.env.ACCEPTANCE_RUST_PORT ?? 3412);
assert.ok(Number.isInteger(port) && port >= 1 && port <= 65535);
const origin = `http://127.0.0.1:${port}`;
const directory = process.env.MEDIA_OFFICIAL_DIR;
const tariff = process.env.MEDIA_TARIFF_FILE;
const reportDir = process.env.MEDIA_REPORT_DIR;
assert.ok(directory && tariff, '设置 MEDIA_OFFICIAL_DIR 和 MEDIA_TARIFF_FILE 后运行。');

const centers = {
  bank: '上海市浦东新区陆家嘴环路1088号 招商银行上海分行',
  health: '上海市黄浦区斜土路547号 安利上海旗舰体验馆',
  hospital: '上海市长宁区平塘路699号 上海和睦家医院',
  park: '上海市浦东新区川沙镇黄赵路310号 上海迪士尼乐园',
};
const connections = {
  hospital: '上海市长宁区淞虹路377号 长宁ArtPark大融城',
  park: '上海市徐汇区华山路1954号 上海交通大学徐汇校区',
};
const coverage = '覆盖媒体_公交地铁_上海市浦东新区川沙镇黄赵路310号上海迪士尼乐园_1000米_20261121.csv';
const connectionCoverage = '覆盖媒体_公交地铁_上海市长宁区平塘路699号上海和睦家医院_1000米_20261121.csv';
const files = await Promise.all([
  ...[3, 4, 5].map((index) => ['bus_stock', join(directory, `公交车身媒体库存 (${index}).xlsx`)]),
  ['metro_stock', join(directory, '地铁出入口媒体库存 (1).xlsx')],
  ['metro_specs', join(directory, '地铁出入口 (1).xlsx')],
  ['rates', tariff],
].map(async ([field, path]) => [field, new Blob([await readFile(path)]), basename(path)]));
const coverBlobs = new Map(await Promise.all([coverage, connectionCoverage]
  .map(async (name) => [name, new Blob([await readFile(join(directory, name))])])));

async function run(path, options) {
  return fetch(origin + path, { signal: AbortSignal.timeout(12000), ...options });
}

async function importCenter(industry, center, connection = '') {
  const form = new FormData();
  for (const [name, value] of Object.entries({
    industry, cover_center: center, starts: '2027-07-01', ends: '2027-07-31',
    observed_at: '2026-11-21T12:01', radius: '1000', attested: 'yes',
    supplemental_metro_center: connection,
  })) form.set(name, value);
  for (const [field, blob, filename] of files) form.append(field, blob, filename);
  const file = connection ? connectionCoverage : coverage;
  form.append('combined_coverage', coverBlobs.get(file), file);
  return run('/api/media-proposals/import', {
    method: 'POST', headers: { Origin: origin }, body: form, redirect: 'manual',
  });
}

const mismatch = await importCenter('bank', '原试算误定位的乐园');
assert.equal(mismatch.status, 422);
assert.match(await mismatch.text(), /没有该中心点的可用媒体/u);
const duplicate = await importCenter('hospital', centers.hospital, centers.hospital);
assert.equal(duplicate.status, 422);
assert.match(await duplicate.text(), /须与主目标圈使用不同中心点/u);

if (reportDir) await mkdir(reportDir, { recursive: true });
for (const [industry, center] of Object.entries(centers)) {
  const connection = connections[industry];
  const uploaded = await importCenter(industry, center, connection);
  assert.equal(uploaded.status, 303, await uploaded.text());
  const source = uploaded.headers.get('location');
  assert.match(source ?? '', /&batch=[0-9a-f-]{36}$/u);
  let saved = false;
  console.log(industry, center);
  for (const medium of ['both', 'bus', 'metro']) {
    for (const amount of [100, 200, 500]) {
      const path = source.replace(/media=\w+/u, 'media=' + medium)
        + '&budget=' + amount + '&compare=100';
      const page = await run(path);
      assert.equal(page.status, 200);
      const html = await page.text();
      for (const [other, name] of Object.entries(centers)) {
        if (other !== industry) assert.ok(!html.includes(name), '混入其他行业覆盖数据');
      }
      const ceiling = html.match(/费用区间<\/dt><dd>¥(\d+)–(\d+)/u);
      assert.ok(ceiling && Number(ceiling[2]) <= amount * 10_000, path);
      const quantities = html.match(/公交 (\d+) 辆 · 地铁 (\d+) 处/u);
      assert.ok(quantities, path);
      const exportPath = html.match(/href="([^"]+)" download>导出当前方案 HTML/u)?.[1]
        ?.replace(/&(?:amp|#38);/gu, '&');
      const canExport = medium === 'both'
        ? Number(quantities[1]) > 0 && Number(quantities[2]) > 0
        : Number(medium === 'bus' ? quantities[1] : quantities[2]) > 0;
      assert.equal(Boolean(exportPath), canExport, path);
      assert.ok(exportPath, '本批赛题快照应在所有预算下支持三种投放方式');
      assert.match(html, /预算占用（按刊例上限）/u);
      if (industry === 'park' && medium === 'both' && amount === 200) {
        assert.match(html, /车辆从 10 辆调至 9 辆 · 地铁点位从 1 处调至 4 处/u);
        assert.match(html, /新组合纳入更多地铁点位，并调减部分公交数量/u);
      }
      if (industry === 'hospital' && medium !== 'bus') {
        assert.match(html, /淞虹路5号口 · 资源规格标记为需确认/u);
      }
      if (connection) {
        assert.ok(html.includes(connection), '缺少补充圈说明');
        if (medium === 'bus') {
          const details = html.match(/<div class="result-header">[\s\S]*?<\/section>\s*<section[^>]+media-items[^>]*>([\s\S]*?)<\/section>/u);
          assert.ok(details && !details[1].includes(connection), '补充圈只能参与地铁选址');
        }
      }
      console.log(`  ${medium} ${amount}万: 公交 ${quantities[1]}，地铁 ${quantities[2]}，上限 ${ceiling[2]}${exportPath ? '' : '（所选媒体未齐）'}`);
      if (exportPath && amount === 200) {
        const report = await run(exportPath);
        assert.equal(report.status, 200);
        const reportHtml = await report.text();
        assert.ok(reportHtml.includes(medium === 'metro' && connection ? connection : center), '导出缺少目标圈来源');
        assert.doesNotMatch(reportHtml, /<script\s|<link\b/u);
        if (reportDir && !saved) {
          await writeFile(join(reportDir, `2027-07-${industry}-${medium}.html`), reportHtml);
          saved = true;
        }
      }
    }
  }
}
