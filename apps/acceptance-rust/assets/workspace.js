{
  const root = document.documentElement;
  const system = matchMedia('(prefers-color-scheme: dark)');
  const themeKey = 'relay.theme.demo';
  const sidebarKey = 'relay.sidebar.demo';
  function read(key) {
    try { return localStorage.getItem(key); } catch { return null; }
  }
  function save(key, value) {
    try { localStorage.setItem(key, value); } catch { /* Current page still works. */ }
  }

  const saved = read(themeKey);
  if (saved === 'light' || saved === 'dark') root.dataset.theme = saved;
  root.toggleAttribute('data-sidebar-collapsed', read(sidebarKey) === 'collapsed');

  document.addEventListener('DOMContentLoaded', () => {
    const theme = document.querySelector('.theme-toggle');
    const sidebar = document.querySelector('.sidebar-toggle');
    if (!theme || !sidebar) return;
    const columnMenu = document.getElementById('sidebar-visibility');
    const columnTrigger = document.querySelector('.sidebar-columns');
    if (typeof columnMenu?.showPopover === 'function' && columnTrigger) {
      for (const link of document.querySelectorAll('[data-optional-column]')) {
        const column = link.dataset.optionalColumn;
        const choice = columnMenu.querySelector(`[data-column-choice="${column}"]`);
        if (!choice) continue;
        const key = 'cityline.column.' + column;
        choice.checked = read(key) !== 'hidden';
        link.hidden = !choice.checked;
        choice.addEventListener('change', () => {
          link.hidden = !choice.checked;
          save(key, choice.checked ? 'visible' : 'hidden');
        });
      }
      columnMenu.hidden = false;
      columnTrigger.hidden = false;
    }
    function synchronizeTheme() {
      const mode = root.dataset.theme ?? (system.matches ? 'dark' : 'light');
      theme.dataset.mode = mode;
      const label = mode === 'dark' ? '切换到浅色模式' : '切换到深色模式';
      theme.setAttribute('aria-label', label);
      theme.title = label;
      for (const input of document.querySelectorAll('[data-theme-options] input')) {
        input.checked = input.value === (root.dataset.theme ?? 'system');
      }
    }
    function bindTask(scope, signal) {
      const taskProgress = scope.querySelector('[data-task-progress][data-active]');
      if (!taskProgress) return;
      const summary = scope.querySelector('[data-task-summary]');
      const status = taskProgress.querySelector('[data-task-status]');
      const error = taskProgress.querySelector('[data-task-error]');
      const steps = Array.from(taskProgress.querySelectorAll('[data-task-step]'), (choice) => {
        const step = choice.labels[0];
        return {
          index: Number(choice.dataset.taskStep),
          choice,
          step,
          label: step.querySelector('[data-step-state]'),
          notes: document.getElementById(choice.getAttribute('aria-controls')).querySelector('[data-step-notes]'),
        };
      }).sort((a, b) => a.index - b.index);
      let currentStep = steps.findIndex(({ choice }) => choice.defaultChecked);
      let followProgress = steps[currentStep].choice.checked;
      for (const [index, { choice }] of steps.entries()) {
        choice.addEventListener('change', () => { followProgress = index === currentStep; }, { signal });
      }
      const delivery = scope.querySelector('[data-task-delivery]');
      const ready = delivery.querySelector('[data-task-ready]');
      const review = delivery.querySelector('[data-task-review]');
      const budgetConfirmation = review?.querySelector('[data-budget-confirmation]');
      const generationTime = delivery.querySelector('[data-generation-time]');
      const agentTraceLink = scope.querySelector('[data-agent-trace-link]');
      const scheduleRevisionLink = scope.querySelector('[data-schedule-revision-link]');
      const blocked = delivery.querySelector('[data-task-blocked]');
      const publicationChecklist = delivery.querySelector('[data-publication-checklist]');
      const reportLinks = delivery.querySelector('[data-task-report-links]');
      const formalDelivery = reportLinks.querySelector('[data-formal-delivery]');
      const platformOutcome = delivery.querySelector('[data-platform-outcome]');
      const login = scope.querySelector('[data-platform-dialog]');
      const showLogin = scope.querySelector('[data-show-platform-dialog]');
      const evaluation = delivery.querySelector('[data-task-evaluation]');
      const localSnapshot = delivery.querySelector('[data-local-snapshot]');
      const operationsReview = delivery.querySelector('[data-ops-review]');
      const proofRefresh = delivery.querySelector('[data-proof-refresh]');
      const goalConfirmation = delivery.querySelector('[data-goal-confirmation]');
      const routeReplacement = delivery.querySelector('[data-route-replacement]');
      const recoverySnapshots = localSnapshot?.querySelector('[data-recovery-snapshots]');
      const sameInventory = localSnapshot?.querySelector('[data-same-inventory]');
      const platformChecks = Array.from(login?.querySelectorAll('[data-platform-check]') ?? []);
      let loginShown = false;
      function showPlatform(phase) {
        if (!login || loginShown || !phase || phase.startsWith('error_') ||
            platformChecks.every((item) => item.textContent.includes('✓'))) return;
        loginShown = true;
        showLogin.hidden = false;
        showLogin.addEventListener('click', () => {
          if (!login.open) login.showModal();
        }, { signal });
        login.showModal();
      }
      showPlatform(login?.dataset.platformDialog);
      async function updateProgress() {
        if (signal.aborted) return;
        let delay = 900;
        try {
          const response = await fetch(taskProgress.dataset.taskProgress, { signal });
          if (!response.ok || response.redirected) throw new Error('无法读取任务。');
          const progress = await response.json();
          if (progress.steps.length !== steps.length || !steps[progress.current_step]) throw new Error('无法读取任务步骤。');
          error.hidden = true;
          if (summary.textContent !== progress.summary) summary.textContent = progress.summary;
          if (status.textContent !== progress.status) status.textContent = progress.status;
          if (agentTraceLink) agentTraceLink.hidden = !progress.has_agent_trace;
          if (login && progress.login) {
            for (const item of platformChecks) {
              const label = progress.login[item.dataset.platformCheck] ? '✓ 已认证' : '待认证';
              if (item.textContent !== label) item.textContent = label;
            }
            if (platformChecks.every((item) => progress.login[item.dataset.platformCheck]) || progress.state !== 'active') {
              showLogin.hidden = true;
              if (login.open) login.close();
            } else {
              showPlatform(progress.login.phase);
            }
          }
          for (const [index, snapshot] of progress.steps.entries()) {
            const { step, label, notes } = steps[index];
            step.dataset.state = snapshot.state;
            if (label.textContent !== snapshot.label) label.textContent = snapshot.label;
            for (const text of snapshot.notes.slice(notes.children.length)) {
              const item = document.createElement('li');
              item.textContent = text;
              notes.append(item);
            }
          }
          currentStep = progress.current_step;
          if (followProgress) steps[currentStep].choice.checked = true;
          if (progress.state !== 'active') {
            ready.hidden = !progress.offer_report;
            if (review) review.hidden = !progress.offer_review;
            if (budgetConfirmation) budgetConfirmation.hidden = !progress.budget_confirmation_required;
            if (generationTime && progress.generation_time) {
              generationTime.textContent = '本版本生成耗时 ' + progress.generation_time;
              generationTime.hidden = false;
            }
            blocked.hidden = progress.offer_report;
            if (publicationChecklist) publicationChecklist.hidden = !progress.offer_proof_refresh;
            reportLinks.hidden = !progress.offer_report;
            if (formalDelivery) formalDelivery.hidden = !progress.offer_formal_delivery;
            platformOutcome.hidden = !progress.platform_interrupted;
            for (const item of platformOutcome.querySelectorAll('[data-auth-result]')) {
              item.textContent = progress.login[item.dataset.authResult] ? '已确认' : '未确认';
            }
            if (evaluation) evaluation.hidden = !progress.offer_evaluation;
            if (localSnapshot) localSnapshot.hidden = !progress.offer_local_snapshot;
            if (operationsReview) operationsReview.hidden = !progress.offer_operations;
            if (scheduleRevisionLink) scheduleRevisionLink.hidden = !progress.offer_schedule_revision;
            if (proofRefresh) proofRefresh.hidden = !progress.offer_proof_refresh;
            if (goalConfirmation) goalConfirmation.hidden = !progress.offer_goal_confirmation;
            if (routeReplacement) routeReplacement.hidden = !progress.offer_route_replacement;
            if (sameInventory) sameInventory.hidden = !progress.offer_same_inventory;
            if (recoverySnapshots && progress.recovery_snapshots?.length) {
              const task = taskProgress.dataset.taskProgress.split('/').pop();
              recoverySnapshots.replaceChildren(...progress.recovery_snapshots.map(({ id, starts, ends }) => {
                const item = document.createElement('li');
                const link = document.createElement('a');
                link.href = '/media-data?' + new URLSearchParams({ batch: id, task });
                link.textContent = `${starts} 至 ${ends}`;
                item.append(link);
                return item;
              }));
              recoverySnapshots.hidden = false;
            }
            delivery.hidden = false;
            taskProgress.removeAttribute('data-active');
            return;
          }
        } catch {
          if (signal.aborted) return;
          error.hidden = false;
          delay = 2500;
        }
        setTimeout(updateProgress, delay);
      }
      void updateProgress();
    }
    function bindOverview(scope, signal) {
      const queues = scope.querySelector('[data-overview-queues]');
      if (!queues) return;
      const warning = scope.querySelector('[data-overview-warning]');
      let timer;
      const delay = () => queues.hasAttribute('data-running') ? 1800 : 4500;
      async function update() {
        if (signal.aborted) return;
        if (!document.hidden && !queues.contains(document.activeElement)) {
          try {
            const response = await fetch('/overview/queues', { signal });
            if (!response.ok || response.redirected) throw new Error('无法更新总览。');
            const next = new DOMParser().parseFromString(await response.text(), 'text/html')
              .querySelector('[data-overview-queues]');
            if (signal.aborted) return;
            if (!next) throw new Error('无法读取任务队列。');
            if (!queues.contains(document.activeElement) && next.innerHTML !== queues.innerHTML) {
              queues.replaceChildren(...next.childNodes);
              queues.toggleAttribute('data-running', next.hasAttribute('data-running'));
            }
            warning.hidden = true;
          } catch {
            if (signal.aborted) return;
            warning.hidden = false;
          }
        }
        timer = setTimeout(update, warning.hidden ? delay() : 5000);
      }
      timer = setTimeout(update, delay());
      signal.addEventListener('abort', () => clearTimeout(timer), { once: true });
    }
    function synchronizeSidebar() {
      const collapsed = root.hasAttribute('data-sidebar-collapsed');
      sidebar.setAttribute('aria-expanded', String(!collapsed));
      const label = collapsed ? '展开侧栏' : '收起侧栏';
      sidebar.setAttribute('aria-label', label);
      sidebar.title = label;
      if (collapsed && columnMenu?.matches(':popover-open')) columnMenu.hidePopover();
    }

    synchronizeTheme();
    synchronizeSidebar();
    system.addEventListener('change', synchronizeTheme);
    function applyTheme(value) {
      if (value === 'system') delete root.dataset.theme;
      else root.dataset.theme = value;
      save(themeKey, value);
      synchronizeTheme();
    }
    theme.addEventListener('click', () => applyTheme(theme.dataset.mode === 'dark' ? 'light' : 'dark'));
    sidebar.addEventListener('click', () => {
      const collapsed = root.toggleAttribute('data-sidebar-collapsed');
      save(sidebarKey, collapsed ? 'collapsed' : 'expanded');
      synchronizeSidebar();
    });
    theme.hidden = false;
    sidebar.hidden = false;
    requestAnimationFrame(() => root.classList.add('sidebar-ready'));

    let pageControls;
    let navigation;
    let siteRevision = 0;
    const viewport = document.querySelector('.workspace-scroll');
    const breadcrumb = document.querySelector('.breadcrumb');
    const account = document.querySelector('.account-link');
    const accountImage = account.querySelector('img');
    accountImage.addEventListener('error', () => { accountImage.hidden = true; });
    async function synchronizeSite(signal) {
      try {
        const response = await fetch('/api/site', { signal });
        if (!response.ok) return;
        const site = await response.json();
        if (signal.aborted) return;
        const logo = document.querySelector('[data-site-logo]');
        const source = site.logo === '/assets/site-logo.png' ? site.logo + '?v=' + siteRevision : '/assets/cityline.svg';
        if (logo.getAttribute('src') !== source) logo.src = source;
        document.querySelector('link[rel="icon"]').href = source;
        document.querySelector('[data-site-name]').textContent = site.name;
        document.querySelector('[data-site-description]').textContent = site.description;
        root.toggleAttribute('data-site-admin', site.admin === true);
        account.querySelector('[data-account-initial]').textContent = site.initial;
        account.querySelector('.account-name').textContent = site.account_label;
        const label = site.user === site.account_label ? site.user : site.user + ' · ' + site.account_label;
        account.setAttribute('aria-label', '个性化设置 · ' + label);
        account.title = label;
        if (!site.avatar) {
          accountImage.hidden = true;
          accountImage.removeAttribute('src');
        } else if (accountImage.getAttribute('src') !== site.avatar) {
          accountImage.hidden = false;
          accountImage.src = site.avatar;
        }
        document.title = site.name + ' · ' + breadcrumb.querySelector('strong').textContent;
      } catch { /* Keep default branding when the settings call fails. */ }
    }
    function bindContent() {
      pageControls?.abort();
      pageControls = new AbortController();
      const scope = viewport.querySelector('main');
      const { signal } = pageControls;
      for (const image of scope.querySelectorAll('[data-account-avatar] img')) {
        image.addEventListener('error', () => { image.hidden = true; }, { signal });
        if (image.complete && image.naturalWidth === 0) image.hidden = true;
      }
      const options = scope.querySelector('[data-theme-options]');
      if (options) {
        options.disabled = false;
        options.addEventListener('change', (event) => applyTheme(event.target.value), { signal });
      }
      const modelSettings = scope.querySelector('[data-agent-model]');
      if (modelSettings) {
        const setup = modelSettings.elements.namedItem('scope');
        const profiles = Array.from(modelSettings.querySelectorAll('[data-model-profile]'), (panel) => {
          const source = panel.querySelector('[data-model-source]');
          const protocol = panel.querySelector('[data-model-protocol]');
          const custom = panel.querySelector('[data-model-custom]');
          const inherited = panel.querySelector('[data-model-inherited]');
          const endpoint = panel.querySelector('[data-model-endpoint]');
          const keyField = panel.querySelector('[data-agent-key-field]');
          const key = keyField?.querySelector('input');
          let customProtocol = protocol.dataset.modelProtocol;
          const sync = () => {
            const visible = panel.dataset.modelProfile === setup.value;
            const server = source.value === 'environment';
            panel.hidden = !visible;
            custom.hidden = server;
            inherited.hidden = !server;
            protocol.value = server ? modelSettings.dataset.inheritedProtocol : customProtocol;
            const origin = source.value === 'local' ? 'http://127.0.0.1:11434' : 'https://api.example.com';
            endpoint.placeholder = origin + protocol.selectedOptions[0].dataset.path;
            for (const control of panel.querySelectorAll('[name]')) control.disabled = !visible;
            protocol.disabled ||= server;
            for (const input of custom.querySelectorAll('input')) {
              input.disabled ||= server;
              input.required = !server && visible;
            }
            if (keyField) {
              keyField.hidden = server && !inherited.querySelector('input');
              key.disabled ||= keyField.hidden;
            }
          };
          source.addEventListener('change', () => {
            if (key) key.value = '';
            sync();
          }, { signal });
          protocol.addEventListener('change', () => {
            customProtocol = protocol.value;
            if (key) key.value = '';
            sync();
          }, { signal });
          endpoint.addEventListener('input', () => {
            if (key) key.value = '';
          }, { signal });
          return sync;
        });
        const syncProfiles = () => profiles.forEach((sync) => sync());
        for (const radio of setup) radio.addEventListener('change', syncProfiles, { signal });
        syncProfiles();
      }
      const loginSettings = scope.querySelector('[data-login-settings]');
      if (loginSettings) {
        const enabled = loginSettings.elements.namedItem('enabled');
        const mode = loginSettings.querySelector('[data-auth-mode]');
        const fields = loginSettings.querySelector('[data-auth-fields]');
        const syncLoginSettings = () => {
          fields.hidden = !enabled.checked;
          for (const field of fields.querySelectorAll('[data-auth-provider]')) {
            const provider = field.dataset.authProvider;
            field.hidden = provider === 'external' ? mode.value === 'local' : provider !== mode.value;
          }
          for (const control of fields.querySelectorAll('input, select')) {
            control.disabled = fields.hidden || Boolean(control.closest('[data-auth-provider][hidden]'));
          }
        };
        loginSettings.addEventListener('change', syncLoginSettings, { signal });
        syncLoginSettings();
      }
      const extraction = scope.querySelector('[data-brief-extraction]');
      const taskForm = scope.querySelector('[data-task-create]');
      for (const trial of scope.querySelectorAll('[data-trial-fields]')) {
        const form = trial.closest('form');
        const goal = form.querySelector('select[name="brief_campaign_goal"], select[name="goal"]');
        const syncTrial = () => {
          const active = goal.value === '到店';
          trial.hidden = !active;
          for (const input of trial.querySelectorAll('input')) input.disabled = !active;
          const monitoring = form.elements.namedItem('monitoring');
          if (monitoring) monitoring.required = !active;
        };
        goal.addEventListener('change', syncTrial, { signal });
        syncTrial();
      }
      const targetCenters = taskForm?.elements.namedItem('target_centers');
      const coreChoices = taskForm?.querySelector('[data-default-centers]');
      if (targetCenters && coreChoices) {
        const updateCoreChoices = () => {
          const lines = targetCenters.value.split(/\r?\n/u).map(line => line.trim()).filter(Boolean);
          const choices = lines.length ? lines : coreChoices.dataset.defaultCenters.split('|');
          coreChoices.replaceChildren(...Array.from(new Set(choices), name => {
            const item = document.createElement('option');
            item.value = name;
            return item;
          }));
        };
        targetCenters.addEventListener('input', updateCoreChoices, { signal });
        updateCoreChoices();
      }
      for (const range of scope.querySelectorAll('[data-date-range]')) {
        const starts = range.querySelector('[name="starts"]');
        const ends = range.querySelector('[name="ends"]');
        const feedback = range.querySelector('[data-date-feedback]');
        const reset = range.querySelector('[data-reset-dates]');
        const syncDates = () => {
          ends.min = starts.value && starts.validity.valid ? starts.value : starts.min;
          const reversed = ends.validity.rangeUnderflow && ends.value >= starts.min;
          feedback.hidden = !reversed;
          if (reversed) {
            ends.setAttribute('aria-invalid', 'true');
            ends.setAttribute('aria-describedby', feedback.id);
          } else {
            ends.removeAttribute('aria-invalid');
            ends.removeAttribute('aria-describedby');
          }
        };
        for (const input of [starts, ends]) {
          if (typeof input.showPicker !== 'function') continue;
          const control = input.parentElement;
          const picker = control.querySelector('[data-date-picker]');
          control.setAttribute('data-picker-ready', '');
          picker.hidden = false;
          picker.addEventListener('click', () => {
            try { input.showPicker(); } catch {
              control.removeAttribute('data-picker-ready');
              picker.hidden = true;
              input.focus();
            }
          }, { signal });
        }
        reset.hidden = false;
        reset.addEventListener('click', () => {
          if (starts.value === starts.min && ends.value === ends.max) return;
          starts.value = starts.min;
          ends.value = ends.max;
          ends.dispatchEvent(new Event('input', { bubbles: true }));
        }, { signal });
        range.addEventListener('input', syncDates, { signal });
        syncDates();
      }
      if (extraction && taskForm) {
        const inputForm = extraction.querySelector('form');
        const button = inputForm.querySelector('button');
        const status = extraction.querySelector('[data-brief-status]');
        const review = taskForm.querySelector('[data-brief-review]');
        const field = (name) => taskForm.elements.namedItem(name);
        const sample = review.querySelector('[data-fill-campaign-case]');
        sample.addEventListener('click', () => {
          const values = JSON.parse(sample.dataset.fillCampaignCase);
          for (const [name, value] of Object.entries(values)) field(name).value = value;
          field('ends').dispatchEvent(new Event('input', { bubbles: true }));
          field('target_centers').dispatchEvent(new Event('input', { bubbles: true }));
          field('brief_campaign_goal').dispatchEvent(new Event('change', { bubbles: true }));
          field('scheduling_factors').closest('details').open = true;
          taskForm.querySelector('[data-trial-fields]').open = true;
          field('brief_confirmed').disabled = false;
          field('brief_confirmed').checked = false;
        }, { signal });
        taskForm.addEventListener('submit', (event) => {
          const goal = field('brief_campaign_goal');
          const confirmed = field('brief_confirmed');
          const missing = !goal.value ? goal : !confirmed.checked ? confirmed : null;
          if (!missing) return;
          event.preventDefault();
          review.open = true;
          missing.setCustomValidity(missing === goal ? '请选择客户主目标。' : '请先确认客户假设书。');
          missing.reportValidity();
          missing.setCustomValidity('');
        }, { signal });
        inputForm.addEventListener('submit', async (event) => {
          event.preventDefault();
          button.disabled = true;
          status.hidden = false;
          status.textContent = '正在提取需求。';
          try {
            const response = await fetch(inputForm.action, { method: 'POST', body: new FormData(inputForm), signal });
            if (!response.ok) throw new Error(await response.text());
            const summary = await response.json();
            const themeMismatch = summary.outside_theme;
            const budget = field('budget');
            const budgetMismatch = summary.budget && Number(budget.value) !== Number(summary.budget);
            if (summary.starts && summary.ends) {
              field('starts').value = summary.starts;
              field('ends').value = summary.ends;
              field('ends').dispatchEvent(new Event('input', { bubbles: true }));
            }
            field('required_routes').value = summary.required_routes.join('、');
            field('hard_cap_wan').value = summary.budget_cap_wan ?? '';
            field('scheduling_factors').value = '';
            field('target_centers').value = '';
            field('core_circle').value = '';
            field('target_centers').dispatchEvent(new Event('input', { bubbles: true }));
            field('brief_used').value = '1';
            field('brief_customer_profile').value = summary.brief.customer_profile;
            field('brief_objective').value = summary.brief.objective;
            field('brief_campaign_goal').value = summary.brief.campaign_goal;
            field('brief_comparison_focus').value = summary.brief.comparison_focus ?? '';
            field('brief_audience').value = summary.brief.audience;
            field('brief_region').value = summary.brief.region;
            field('brief_monitoring').value = summary.brief.monitoring;
            const trial = summary.brief.trial_measurement;
            field('trial_stores').value = trial?.stores ?? '';
            field('trial_window').value = trial?.window ?? '';
            field('trial_baseline').value = trial?.baseline ?? '';
            field('brief_campaign_goal').dispatchEvent(new Event('change', { bubbles: true }));
            if (trial) taskForm.querySelector('[data-trial-fields]').open = true;
            field('brief_confirmed').checked = false;
            field('brief_confirmed').disabled = Boolean(themeMismatch);
            review.open = true;
            status.textContent = summary.issues.length ? '提取完成，请补齐以下项目。' : '已提取投放条件，请在下方确认。';
            if (summary.subject) {
              const subject = document.createElement('p');
              subject.textContent = summary.subject;
              status.append(subject);
            }
            if (summary.issues.length) {
              const issues = document.createElement('ul');
              for (const text of summary.issues) {
                const issue = document.createElement('li');
                issue.textContent = text;
                issues.append(issue);
              }
              status.append(issues);
            }
            if (budgetMismatch) {
              const budgetNote = document.createElement('p');
              budgetNote.textContent = 'Brief 的预算为 ' + summary.budget + ' 万元。展示案例按约 ' + budget.value + ' 万元生成 A／B 两套方案，请确认后提交。';
              status.append(budgetNote);
            }
            if (summary.bus !== summary.metro) {
              const mediaNote = document.createElement('p');
              const requested = summary.bus ? '公交车身' : '地铁出入口';
              mediaNote.textContent = `Brief 仅列出${requested}。正式提案同时使用公交车身和地铁出入口，请核对后提交。`;
              status.append(mediaNote);
            }
          } catch (error) {
            if (!signal.aborted) status.textContent = error.message || '提取失败，请核对文件。';
          } finally {
            button.disabled = false;
          }
        }, { signal });
        taskForm.addEventListener('input', (event) => {
          if (event.target !== field('brief_confirmed')) field('brief_confirmed').checked = false;
        }, { signal });
      }
      bindTask(scope, signal);
      bindOverview(scope, signal);
      synchronizeTheme();
      void synchronizeSite(signal);
    }
    bindContent();

    async function loadPage(url, { method = 'GET', body, pop = false, form } = {}) {
      navigation?.abort();
      const controller = new AbortController();
      navigation = controller;
      const { signal } = controller;
      try {
        const response = await fetch(url, { method, body, signal });
        if (!response.ok) {
          if (form) {
            let feedback = form.querySelector('[data-form-feedback]');
            if (!feedback) {
              feedback = document.createElement('p');
              feedback.className = 'form-feedback';
              feedback.dataset.tone = 'error';
              feedback.dataset.formFeedback = '';
              feedback.setAttribute('role', 'alert');
              form.append(feedback);
            }
            const message = response.headers.get('content-type')?.startsWith('text/plain')
              ? await response.text() : '操作暂时无法完成，请重试。';
            feedback.textContent = message;
            if (form.matches('[data-task-review]') && [409, 503].includes(response.status)) {
              const current = viewport.querySelector('[data-approval-inventory]');
              try {
                const detail = await fetch(url.pathname.slice(0, -'/review'.length), { signal });
                if (detail.ok && !detail.redirected && !signal.aborted) {
                  const next = new DOMParser().parseFromString(await detail.text(), 'text/html')
                    .querySelector('[data-approval-inventory]');
                  if (next && !signal.aborted) current?.replaceWith(next);
                }
              } catch {
                // Keep the review form and its error message available.
              }
            }
            return;
          }
          location.assign(url);
          return;
        }
        const doc = new DOMParser().parseFromString(await response.text(), 'text/html');
        if (signal.aborted) return;
        const nextContent = doc.querySelector('.workspace-scroll > main');
        const nextBreadcrumb = doc.querySelector('.breadcrumb');
        if (!nextContent || !nextBreadcrumb) {
          location.assign(response.url);
          return;
        }
        document.title = doc.title;
        breadcrumb.replaceChildren(...nextBreadcrumb.childNodes);
        const current = doc.querySelector('#workspace-nav [aria-current="page"]')?.getAttribute('href');
        for (const link of document.querySelectorAll('#workspace-nav a')) {
          if (link.getAttribute('href') === current) link.setAttribute('aria-current', 'page');
          else link.removeAttribute('aria-current');
        }
        viewport.querySelector('main').replaceWith(nextContent);
        if (method === 'POST' && url.pathname === '/settings/brand') siteRevision += 1;
        if (!pop) {
          const nextUrl = new URL(response.url);
          if (nextUrl.href !== location.href) history.pushState({}, '', nextUrl);
        }
        viewport.scrollTop = 0;
        nextContent.focus({ preventScroll: true });
        nextContent.classList.add('workspace-enter');
        bindContent();
      } catch {
        if (signal.aborted) return;
        if (method === 'GET') location.assign(url);
        else form?.dispatchEvent(new Event('cityline-network-error'));
      } finally {
        if (navigation === controller) navigation = null;
      }
    }
    document.addEventListener('click', (event) => {
      if (event.defaultPrevented || event.button !== 0 || event.metaKey || event.ctrlKey || event.shiftKey || event.altKey) return;
      const link = event.target.closest('a[href]');
      if (!link || link.target || link.hasAttribute('download') || link.hasAttribute('data-full-navigation')) return;
      const url = new URL(link.href);
      if (url.origin !== location.origin || url.hash || url.pathname.startsWith('/assets/')) return;
      event.preventDefault();
      void loadPage(url);
    });
    document.addEventListener('change', (event) => {
      if (event.target.matches('select[data-page-size]')) event.target.form?.requestSubmit();
    });
    document.addEventListener('submit', async (event) => {
      const form = event.target;
      if (event.defaultPrevented || !form.matches('form[data-workspace-submit]')) return;
      event.preventDefault();
      if (form.matches('[aria-busy="true"]')) return;
      const fields = new FormData(form, event.submitter);
      if (form.hasAttribute('data-redbean-credentials')) form.elements.namedItem('password').value = '';
      const multipart = form.enctype === 'multipart/form-data';
      const data = multipart ? fields : new URLSearchParams(fields);
      const url = new URL(event.submitter?.getAttribute('formaction') || form.action, location.href);
      const post = form.method.toUpperCase() === 'POST';
      if (!post) url.search = data.toString();
      let failedConnection = false;
      const handleError = () => { failedConnection = true; };
      form.addEventListener('cityline-network-error', handleError, { once: true });
      form.setAttribute('aria-busy', 'true');
      await loadPage(url, { method: post ? 'POST' : 'GET', body: post ? data : undefined, form });
      form.removeAttribute('aria-busy');
      form.removeEventListener('cityline-network-error', handleError);
      if (failedConnection) {
        const message = document.createElement('p');
        message.className = 'form-feedback';
        message.dataset.tone = 'error';
        message.setAttribute('role', 'alert');
        message.textContent = form.matches('[data-task-create]')
          ? '连接中断，请在任务明细确认提交结果。'
          : '连接中断，请刷新页面确认操作结果。';
        form.append(message);
      }
    });
    addEventListener('popstate', () => void loadPage(location.href, { pop: true }));
  });
}
