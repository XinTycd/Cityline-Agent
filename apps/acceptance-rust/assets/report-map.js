(() => {
  const maxScale = 6;
  const clamp = (value, min, max) => Math.min(max, Math.max(min, value));

  function enableMap(figure) {
    let viewport = figure.querySelector(':scope > .report-map-window');
    const map = (viewport || figure).querySelector(':scope > img, :scope > svg');
    if (!map) return;

    if (!viewport) {
      viewport = document.createElement('div');
      viewport.className = 'report-map-window';
      viewport.setAttribute('role', 'group');
      viewport.setAttribute('aria-label', map.querySelector('title')?.textContent || '投放线路图');
      map.before(viewport);
      viewport.append(map);
    }
    viewport.tabIndex = 0;
    viewport.setAttribute('aria-description', '滚轮缩放，放大后可拖动。也可用加减键缩放、方向键移动、Home 键复位。');
    viewport.setAttribute('data-interactive-map', '');
    map.setAttribute('draggable', 'false');

    const controls = document.createElement('div');
    controls.className = 'report-map-controls';
    controls.innerHTML = `<span>滚轮缩放，放大后可拖动</span>
      <div class="report-map-actions" role="group" aria-label="地图缩放">
        <button type="button" class="secondary" aria-label="缩小地图">−</button>
        <span class="report-map-scale">100%</span>
        <button type="button" class="secondary" aria-label="放大地图">+</button>
        <button type="button" class="secondary">复位</button>
      </div>`;
    // Saved reports may still include the earlier fixed-zoom control.
    figure.querySelector(':scope > .report-map-zoom')?.remove();
    viewport.before(controls);
    const [zoomOut, zoomIn, reset] = controls.querySelectorAll('button');
    const scaleLabel = controls.querySelector('.report-map-scale');

    let scale = 1;
    let x = 0;
    let y = 0;
    let drag = null;

    function render() {
      const minOffset = 1 - scale;
      x = clamp(x, minOffset, 0);
      y = clamp(y, minOffset, 0);
      map.style.transform = `translate(${x * 100}%, ${y * 100}%) scale(${scale})`;
      viewport.toggleAttribute('data-map-zoomed', scale > 1);
      zoomOut.disabled = scale === 1;
      zoomIn.disabled = scale === maxScale;
      reset.disabled = scale === 1;
      scaleLabel.textContent = `${Math.round(scale * 100)}%`;
    }

    // Relative offsets preserve the view when the report layout changes.
    function zoom(nextScale, anchorX = 0.5, anchorY = 0.5) {
      const next = clamp(nextScale, 1, maxScale);
      const ratio = next / scale;
      x = anchorX - (anchorX - x) * ratio;
      y = anchorY - (anchorY - y) * ratio;
      scale = next;
      render();
    }

    viewport.addEventListener('wheel', event => {
      if (!event.deltaY || event.ctrlKey) return;
      event.preventDefault();
      const bounds = viewport.getBoundingClientRect();
      const { clientWidth, clientHeight, clientLeft, clientTop } = viewport;
      const unit = event.deltaMode === 1 ? 16 : event.deltaMode === 2 ? clientHeight : 1;
      const anchorX = (event.clientX - bounds.left - clientLeft) / clientWidth;
      const anchorY = (event.clientY - bounds.top - clientTop) / clientHeight;
      zoom(scale * Math.exp(-event.deltaY * unit / 480), anchorX, anchorY);
    }, { passive: false });

    viewport.addEventListener('pointerdown', event => {
      if (scale === 1 || !event.isPrimary || event.button !== 0) return;
      drag = { id: event.pointerId, x: event.clientX, y: event.clientY };
      viewport.setPointerCapture(event.pointerId);
      viewport.setAttribute('data-map-dragging', '');
    });
    viewport.addEventListener('pointermove', event => {
      if (!drag || event.pointerId !== drag.id) return;
      x += (event.clientX - drag.x) / viewport.clientWidth;
      y += (event.clientY - drag.y) / viewport.clientHeight;
      drag.x = event.clientX;
      drag.y = event.clientY;
      render();
    });
    viewport.addEventListener('lostpointercapture', event => {
      if (event.pointerId !== drag?.id) return;
      drag = null;
      viewport.removeAttribute('data-map-dragging');
    });

    const directions = {
      ArrowLeft: [1, 0], ArrowRight: [-1, 0], ArrowUp: [0, 1], ArrowDown: [0, -1],
    };
    viewport.addEventListener('keydown', event => {
      const direction = directions[event.key];
      if (event.key === '+' || event.key === '=') zoom(scale * 1.3);
      else if (event.key === '-') zoom(scale / 1.3);
      else if (event.key === 'Home') zoom(1);
      else if (scale > 1 && direction) {
        x += direction[0] * 0.1;
        y += direction[1] * 0.1;
        render();
      } else return;
      event.preventDefault();
    });

    zoomOut.addEventListener('click', () => zoom(scale / 1.3));
    zoomIn.addEventListener('click', () => zoom(scale * 1.3));
    reset.addEventListener('click', () => zoom(1));
    render();
  }

  document.querySelectorAll('.report-route-map').forEach(enableMap);
})();
