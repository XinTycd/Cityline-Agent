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
          choice,
          step,
          label: step.querySelector('[data-step-state]'),
          notes: document.getElementById(choice.getAttribute('aria-controls')).querySelector('[data-step-notes]'),
        };
      });
      let currentStep = steps.findIndex(({ choice }) => choice.defaultChecked);
      let followProgress = steps[currentStep].choice.checked;
      for (const [index, { choice }] of steps.entries()) {
        choice.addEventListener('change', () => { followProgress = index === currentStep; }, { signal });
      }
      const delivery = scope.querySelector('[data-task-delivery]');
      const ready = delivery.querySelector('[data-task-ready]');
      const blocked = delivery.querySelector('[data-task-blocked]');
      const alternative = delivery.querySelector('[data-task-alternative]');
      const login = scope.querySelector('[data-platform-dialog]');
      const showLogin = scope.querySelector('[data-show-platform-dialog]');
      const evaluation = delivery.querySelector('[data-task-evaluation]');
      const localSnapshot = delivery.querySelector('[data-local-snapshot]');
      const operationsReview = delivery.querySelector('[data-ops-review]');
      const proofRefresh = delivery.querySelector('[data-proof-refresh]');
      const recoverySnapshots = localSnapshot?.querySelector('[data-recovery-snapshots]');
      const sameInventory = localSnapshot?.querySelector('[data-same-inventory]');
      const platformChecks = Array.from(login?.querySelectorAll('[data-platform-check]') ?? []);
      if (login && !platformChecks.every((item) => item.textContent.includes('✓'))) {
        showLogin.hidden = false;
        showLogin.addEventListener('click', () => {
          if (!login.open) login.showModal();
        }, { signal });
        login.showModal();
      }
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
          if (login && progress.login) {
            for (const item of platformChecks) {
              const label = progress.login[item.dataset.platformCheck] ? '✓ 已认证' : '待认证';
              if (item.textContent !== label) item.textContent = label;
            }
            if (platformChecks.every((item) => progress.login[item.dataset.platformCheck]) || progress.state !== 'active') {
              showLogin.hidden = true;
              if (login.open) login.close();
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
            taskProgress.removeAttribute('data-active');
            ready.hidden = progress.state !== 'done';
            blocked.hidden = progress.state === 'done' || !progress.has_alternative;
            alternative.hidden = !progress.has_alternative;
            if (evaluation) evaluation.hidden = !progress.offer_evaluation;
            if (localSnapshot) localSnapshot.hidden = !progress.offer_local_snapshot;
            if (operationsReview) operationsReview.hidden = !progress.offer_operations;
            if (proofRefresh) proofRefresh.hidden = !progress.offer_proof_refresh;
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
        const mode = modelSettings.elements.namedItem('mode');
        const fields = modelSettings.querySelector('[data-agent-model-fields]');
        const keyField = modelSettings.querySelector('[data-agent-key-field]');
        const environmentNote = modelSettings.querySelector('[data-agent-environment-note]');
        const hasEnvironmentModel = modelSettings.hasAttribute('data-agent-environment-configured');
        const showInheritedModel = mode.value === 'environment' && hasEnvironmentModel;
        const syncModelSettings = () => {
          const editable = mode.value === 'cloud' || mode.value === 'local';
          const environment = mode.value === 'environment';
          fields.hidden = !(editable || (environment && showInheritedModel));
          environmentNote.hidden = !environment || showInheritedModel;
          for (const input of fields.querySelectorAll('input')) {
            input.readOnly = !editable;
            input.required = editable;
          }
          if (keyField) {
            keyField.hidden = !(editable || (environment && hasEnvironmentModel));
            keyField.querySelector('input').disabled = keyField.hidden;
          }
        };
        mode.addEventListener('change', () => {
          if (keyField) keyField.querySelector('input').value = '';
          syncModelSettings();
        }, { signal });
        syncModelSettings();
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
            field('brief_used').value = '1';
            field('brief_customer_profile').value = summary.brief.customer_profile;
            field('brief_objective').value = summary.brief.objective;
            field('brief_campaign_goal').value = summary.brief.campaign_goal;
            field('brief_audience').value = summary.brief.audience;
            field('brief_region').value = summary.brief.region;
            field('brief_monitoring').value = summary.brief.monitoring;
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
      if (!form.matches('form[data-workspace-submit]')) return;
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
