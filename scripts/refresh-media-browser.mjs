import { mkdir, readFile, rename, writeFile } from 'node:fs/promises';
import { join } from 'node:path';
import { cloudClient, cloudOrigin, cloudPath, redClient, redOrigin, redPath, archive } from './contest-http.mjs';
import { acquireDirectEvidence } from './contest-direct-evidence.mjs';
import { captureContestMap } from './contest-map-screenshot.mjs';

const [port, directory, tariff] = process.argv.slice(2);
if (!/^\d{1,5}$/u.test(port) || Number(port) < 1 || Number(port) > 65535 || !directory) {
  throw new Error('Invalid local refresh configuration');
}
const origin = 'http://127.0.0.1:' + port;
const request = JSON.parse(await readFile(join(directory, 'request.json'), 'utf8'));
const { industry, center, media, radius, supplementalCenter, requiredRoutes } = request;
const proofOnly = request.proofOnly === true;
const primaryCenters = typeof center === 'string' ? center.split('|') : [];
const activeSupplement = supplementalCenter && media !== 'bus' ? supplementalCenter : null;
const requested = request.periods;
if ((!proofOnly && !tariff) || industry !== 'food'
  || !['bus', 'metro', 'both'].includes(media) || ![500, 1000, 1500].includes(radius)
  || !primaryCenters.length || primaryCenters.length > 8 || primaryCenters.some((name) => !name || name.length > 100)
  || (activeSupplement && (typeof activeSupplement !== 'string' || activeSupplement.length > 100))
  || !Array.isArray(requiredRoutes) || requiredRoutes.length > 12
  || requiredRoutes.some((route) => typeof route !== 'string' || !/^[\p{L}\p{N}()（）-－]{1,26}$/u.test(route))
  || !Array.isArray(requested) || requested.length === 0 || requested.length > 12 || requested.some(({ starts, ends }) =>
    !/^2027-\d{2}-\d{2}$/u.test(starts) || !/^2027-\d{2}-\d{2}$/u.test(ends) || starts > ends)) {
  throw new Error('Invalid platform task');
}
const headers = { Origin: origin, 'X-Cityline-Media-Import': process.env.CITYLINE_MEDIA_IMPORT_TOKEN };
if (!headers['X-Cityline-Media-Import']) throw new Error('Missing task authorization');
const savedCloudSession = process.env.CITYLINE_NEWCLOUD_SESSION;
const savedRedToken = process.env.CITYLINE_REDBEAN_ACCESS_TOKEN;
const savedRedTenant = process.env.CITYLINE_REDBEAN_TENANT_ID || '1';
delete process.env.CITYLINE_MEDIA_IMPORT_TOKEN;
delete process.env.CITYLINE_NEWCLOUD_SESSION;
delete process.env.CITYLINE_REDBEAN_ACCESS_TOKEN;
delete process.env.CITYLINE_REDBEAN_TENANT_ID;
const authenticated = { newcloud: false, redbean: false };
const inventory = { api: 0, exports: 0 };
let redbeanFallback = false;
let currentPeriod = null;
let browser;
let captureContext;
let phase = 'launch';
let pendingStatus = Promise.resolve();

async function status(state, report = null, evidence = null) {
  const payload = JSON.stringify({ state, report, period: currentPeriod, evidence, inventory, redbeanFallback, ...authenticated });
  const path = join(directory, 'status.json');
  pendingStatus = pendingStatus.catch(() => {}).then(async () => {
    await writeFile(path + '.tmp', payload, 'utf8');
    await rename(path + '.tmp', path);
  });
  await pendingStatus;
}

async function openLogin(page, target) {
  try {
    const result = await page.goto(target, { waitUntil: 'domcontentloaded', timeout: 16000 });
    if (result && result.status() < 400) return;
  } catch {}
  const error = new Error('Login page unavailable');
  error.platformStage = 'connection';
  throw error;
}

async function mark(which, result) {
  authenticated[which] = true;
  await status('login');
  return result;
}

async function sessions() {
  let cloudSession = savedCloudSession;
  if (proofOnly && cloudSession) {
    const cloud = cloudClient(cloudSession);
    try {
      await cloud.routeCatalog();
      return [await mark('newcloud', cloud), null];
    } catch {
      cloudSession = null;
    }
  }
  const dates = { startDate: requested[0].starts, endDate: requested[0].ends };
  const verifyRed = async (token, tenant) => {
    let client;
    try {
      client = redClient(token, tenant);
      await client.authorize(dates, media);
    } catch {
      const error = new Error('Redbean did not confirm inventory access');
      error.platformStage = 'redbean_auth';
      throw error;
    }
    return mark('redbean', client);
  };
  const cachedRed = !proofOnly && savedRedToken
    ? verifyRed(savedRedToken, savedRedTenant).catch(() => null)
    : Promise.resolve(null);
  if (cloudSession) {
    const verified = await cachedRed;
    if (verified) return [cloudClient(cloudSession), verified];
  }
  // The visible pages are for operator-completed sign-in.
  phase = 'launch';
  const { chromium } = await import('playwright-core');
  browser = await chromium.launch({ channel: 'msedge', headless: false });
  const context = await browser.newContext();
  captureContext = context;
  const newcloud = cloudSession ? null : await context.newPage();
  if (cloudSession) await addCloudCookies(context, cloudSession);
  phase = 'login';
  await status('login');

  return Promise.all([
    newcloud ? (async () => {
      await openLogin(newcloud, cloudOrigin + cloudPath('LOGIN'));
      await newcloud.bringToFront();
      await newcloud.waitForFunction(
        ({ origin, landing, marker }) => location.origin === origin && location.pathname === landing
          && document.getElementById(marker)?.textContent.includes('媒体覆盖分析'),
        { origin: cloudOrigin, landing: cloudPath('LANDING'), marker: process.env.CITYLINE_NEWCLOUD_READY_MARKER },
        { timeout: 240000 },
      );
      const session = (await context.cookies(cloudOrigin)).map(({ name, value }) => name + '=' + value).join('; ');
      return mark('newcloud', cloudClient(session));
    })() : cloudClient(cloudSession),
    proofOnly ? null : (async () => {
      const verified = await cachedRed;
      if (verified) return verified;
      const redbean = await context.newPage();
      await openLogin(redbean, redOrigin + redPath('LOGIN'));
      if (savedRedToken) {
        redbeanFallback = true;
        await status('login');
      }
      if (!newcloud || authenticated.newcloud) await redbean.bringToFront();
      await redbean.waitForFunction((origin) => {
        if (location.origin !== origin || /\/login\/?$/u.test(location.pathname)) return false;
        return Boolean(localStorage.getItem('ACCESS_TOKEN') && localStorage.getItem('tenantId'));
      }, redOrigin, { timeout: 240000 });
      const result = await redbean.evaluate((origin) => {
        if (location.origin !== origin) return null;
        const read = (key) => {
          const item = JSON.parse(localStorage.getItem(key));
          return item?.e > Date.now() ? JSON.parse(item.v) : null;
        };
        return { token: read('ACCESS_TOKEN'), tenant: String(read('tenantId') ?? '') };
      }, redOrigin);
      return verifyRed(result?.token, result?.tenant);
    })(),
  ]);
}

async function addCloudCookies(context, cookie) {
  const pairs = cookie.split(/;\s*/u).map((pair) => {
    const equal = pair.indexOf('=');
    return { name: pair.slice(0, equal), value: pair.slice(equal + 1), url: cloudOrigin };
  });
  await context.addCookies(pairs);
}

async function screenshotContext() {
  if (captureContext) return captureContext;
  const { chromium } = await import('playwright-core');
  browser = await chromium.launch({ channel: 'msedge', headless: true });
  captureContext = await browser.newContext();
  await addCloudCookies(captureContext, savedCloudSession);
  return captureContext;
}

async function exportInventory(red, medium, dates) {
  const label = medium === 'bus' ? '公交' : '地铁';
  const bytes = medium === 'bus' ? await red.busExport(dates) : await red.metroExport(dates);
  return [medium + '_stock', '红豆官方导出-' + label + '库存.xlsx', bytes];
}

async function readInventory(red, medium, dates) {
  const label = medium === 'bus' ? '公交' : '地铁';
  try {
    const bytes = medium === 'bus' ? await red.bus(dates, requiredRoutes) : await red.metro(dates);
    return [medium + '_stock', '红豆库存API-' + label + '.csv', bytes];
  } catch (error) {
    if (/returned HTTP (401|403)\b/u.test(error?.message ?? '')) throw error;
    return exportInventory(red, medium, dates);
  }
}

async function importSources(window, acquired) {
  const { starts, ends } = window;
  const observed = new Date().toLocaleString('sv-SE', { timeZone: 'Asia/Shanghai' }).replace(' ', 'T').slice(0, 16);
  const body = new FormData();
  for (const [key, value] of Object.entries({ industry, starts, ends, observed_at: observed, attested: 'yes', cover_center: center })) {
    body.set(key, value);
  }
  if (requiredRoutes.length) body.set('required_routes', requiredRoutes.join('、'));
  if (activeSupplement) body.set('supplemental_metro_center', activeSupplement);
  for (const [field, name, bytes] of acquired) body.append(field, new Blob([bytes]), name);
  body.set('rates', new Blob([await readFile(tariff)]), '07-媒体刊例价.xlsx');
  const response = await fetch(origin + '/api/media-proposals/import', {
    method: 'POST', body, headers, redirect: 'manual', signal: AbortSignal.timeout(20000),
  });
  const result = response.headers.get('location');
  if (response.status !== 303 || !result?.startsWith('/media-proposals?')) {
    const error = new Error('Sources did not pass local import (HTTP ' + response.status + ')');
    error.status = response.status;
    throw error;
  }
  const params = new URLSearchParams(result.split('?')[1]);
  if (params.get('industry') !== industry || params.get('starts') !== starts || params.get('ends') !== ends
    || params.get('media') !== media || !/^[0-9a-f-]{36}$/u.test(params.get('batch') ?? '')) {
    throw new Error('Unexpected snapshot receipt');
  }
  params.set('budget', String(request.budget ?? 200));
  return '/media-plans?' + params;
}

async function collectInventory(cloud, red) {
  phase = 'coverage';
  await status('coverage');
  const centers = activeSupplement ? [...primaryCenters, activeSupplement] : primaryCenters;
  const coverage = await cloud.coverage(centers, radius, media, Boolean(activeSupplement));
  authenticated.newcloud = true;
  await archive(directory, '新云接口覆盖原始响应.json', JSON.stringify(coverage.rows));
  await archive(directory, '新云接口覆盖.csv', coverage.csv);
  const coverageFile = ['combined_coverage', '新云接口覆盖.csv', coverage.csv];
  let metroSpecs;
  let receipt;
  const missed = [];

  for (const window of requested) {
    const { starts, ends } = window;
    const dates = { startDate: starts, endDate: ends };
    currentPeriod = starts + ' 至 ' + ends;
    phase = 'inventory';
    await status(requiredRoutes.length ? 'required_inventory' : 'inventory');
    try {
      const exports = [coverageFile];
      if (media !== 'metro') exports.push(await readInventory(red, 'bus', dates));
      if (media !== 'bus') {
        exports.push(await readInventory(red, 'metro', dates));
        if (!metroSpecs) metroSpecs = await red.specs();
        exports.push(['metro_specs', '红豆接口地铁规格.xlsx', metroSpecs]);
      }
      authenticated.redbean = true;
      for (const [, name, bytes] of exports.slice(1)) await archive(directory, starts + '-' + name, bytes);
      phase = 'import';
      await status('importing');
      const isApi = ([, name]) => name.startsWith('红豆库存API-');
      let sources = exports;
      try {
        receipt = await importSources(window, sources);
      } catch (error) {
        if (error.status !== 422 || !sources.some(isApi)) throw error;
        sources = await Promise.all(exports.map(async (source) => {
          if (!isApi(source)) return source;
          const fallback = await exportInventory(red, source[0] === 'bus_stock' ? 'bus' : 'metro', dates);
          await archive(directory, starts + '-' + fallback[1], fallback[2]);
          return fallback;
        }));
        receipt = await importSources(window, sources);
      }
      inventory.api += sources.filter(isApi).length;
      inventory.exports += sources.filter(([, name]) => name.startsWith('红豆官方导出-')).length;
    } catch (error) {
      if (/returned HTTP (401|403)\b/u.test(error?.message ?? '')) throw error;
      missed.push({ starts, ends, phase, error: error?.message?.slice(0, 180) });
    }
  }
  if (missed.length) await archive(directory, '待补档期.json', JSON.stringify(missed, null, 2));
  if (!receipt) throw new Error('No saleable stock snapshot could be imported for the selected dates');
  return { receipt, missed };
}

try {
  await mkdir(directory, { recursive: true });
  await status('opening');
  const [cloud, red] = await sessions();
  const { receipt, missed } = proofOnly
    ? { receipt: null, missed: [] }
    : await collectInventory(cloud, red);
  phase = 'proof';
  currentPeriod = null;
  await status('proof');
  let checkpoint = null;
  try {
    const evidence = await acquireDirectEvidence(cloud, directory, origin, headers, async (plan, overview) => {
      currentPeriod = plan.starts + ' 至 ' + plan.ends;
      checkpoint = { ...overview };
      await status('proof', null, checkpoint);
    }, async (routes, catalog, filename) => captureContestMap(await screenshotContext(), directory, routes, catalog, filename));
    const complete = !missed.length && !evidence.unavailablePeriods
      && evidence.maps === evidence.expectedMaps && evidence.reach === evidence.expectedReach;
    await status(complete ? 'done' : 'needs_review', receipt, evidence);
  } catch (error) {
    await archive(directory, '取数诊断.json', JSON.stringify({ phase, error: error?.message?.slice(0, 180) }));
    await status('needs_review', receipt, checkpoint);
  }
} catch (error) {
  await archive(directory, '取数诊断.json', JSON.stringify({ phase, error: error?.message?.slice(0, 180) })).catch(() => {});
  await status('error_' + (error?.platformStage ?? phase));
  process.exitCode = 1;
} finally {
  if (browser) await browser.close();
}
