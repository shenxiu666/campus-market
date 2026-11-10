/* ==========================================================================
 * 校园二手交易推荐 · mini-redis 演示界面
 *
 * 纯手写，零第三方依赖（无 React / Vue / Chart.js / CDN）。
 * 四个视图：市场 / 详情·发布 / 为你推荐 / 演示面板，单页无刷新切换。
 * ========================================================================== */
'use strict';

(function () {
  /* ------------------------------------------------------------------ *
   * 常量与状态
   * ------------------------------------------------------------------ */
  const PAGE_SIZE = 12;
  const POLL_MS = 3000;
  const VIEWS = ['market', 'detail', 'recommend', 'demo'];

  const state = {
    view: 'market',
    products: [],
    total: 0,
    offset: 0,
    tags: [],
    filters: { sort: 'time_desc', q: '', min: '', max: '', tags: [] },
    users: [],
    currentProduct: null,
    pollTimer: null,
    benchDone: false,
  };

  const $ = (sel, root) => (root || document).querySelector(sel);
  const $$ = (sel, root) => Array.from((root || document).querySelectorAll(sel));

  /* ------------------------------------------------------------------ *
   * 工具函数
   * ------------------------------------------------------------------ */
  function el(tag, attrs, children) {
    const node = document.createElement(tag);
    if (attrs) {
      Object.keys(attrs).forEach((key) => {
        const value = attrs[key];
        if (value === null || value === undefined || value === false) return;
        if (key === 'class') node.className = value;
        else if (key === 'text') node.textContent = value;
        else if (key === 'html') node.innerHTML = value;
        else if (key.startsWith('on')) node.addEventListener(key.slice(2).toLowerCase(), value);
        else node.setAttribute(key, value);
      });
    }
    (children || []).forEach((child) => {
      if (child === null || child === undefined) return;
      node.appendChild(typeof child === 'string' ? document.createTextNode(child) : child);
    });
    return node;
  }

  function escapeHtml(text) {
    return String(text === null || text === undefined ? '' : text)
      .replace(/&/g, '&amp;')
      .replace(/</g, '&lt;')
      .replace(/>/g, '&gt;')
      .replace(/"/g, '&quot;')
      .replace(/'/g, '&#39;');
  }

  /** 分 → "450.00" */
  function yuan(cents) {
    const value = Number(cents) || 0;
    return (value / 100).toFixed(2);
  }

  /** 元 → 分（四舍五入） */
  function toCents(input) {
    const value = Number(input);
    if (!isFinite(value)) return NaN;
    return Math.round(value * 100);
  }

  function timeAgo(seconds) {
    const ts = Number(seconds) || 0;
    if (!ts) return '—';
    const diff = Math.max(0, Math.floor(Date.now() / 1000) - ts);
    if (diff < 60) return diff + ' 秒前';
    if (diff < 3600) return Math.floor(diff / 60) + ' 分钟前';
    if (diff < 86400) return Math.floor(diff / 3600) + ' 小时前';
    if (diff < 86400 * 30) return Math.floor(diff / 86400) + ' 天前';
    const date = new Date(ts * 1000);
    return date.getFullYear() + '-' + pad(date.getMonth() + 1) + '-' + pad(date.getDate());
  }

  function pad(n) {
    return n < 10 ? '0' + n : String(n);
  }

  function fmtNumber(n) {
    return Number(n || 0).toLocaleString('zh-CN');
  }

  /** 商品状态枚举 → 中文文案 */
  function statusLabel(status) {
    return ({ onsale: '在售', reserved: '已预订', sold: '已售出' })[status] || status || '在售';
  }

  function fmtMs(ms) {
    const value = Number(ms) || 0;
    if (value >= 1000) return (value / 1000).toFixed(2) + ' s';
    if (value >= 10) return value.toFixed(1);
    return value.toFixed(3);
  }

  /* ------------------------------------------------------------------ *
   * Toast 提示
   * ------------------------------------------------------------------ */
  function toast(message, kind) {
    const wrap = $('#toast-wrap');
    if (!wrap) return;
    const node = el('div', { class: 'toast ' + (kind || ''), text: message });
    wrap.appendChild(node);
    setTimeout(() => {
      node.classList.add('out');
      setTimeout(() => node.remove(), 240);
    }, kind === 'err' ? 4200 : 2600);
  }

  /* ------------------------------------------------------------------ *
   * fetch 封装：统一错误处理 + 中文提示
   * ------------------------------------------------------------------ */
  async function api(path, options) {
    const opts = options || {};
    let response;
    try {
      response = await fetch(path, {
        method: opts.method || 'GET',
        headers: opts.body ? { 'content-type': 'application/json' } : undefined,
        body: opts.body ? JSON.stringify(opts.body) : undefined,
      });
    } catch (err) {
      throw new Error('网络错误：无法连接到网关（' + err.message + '）');
    }

    let payload = null;
    const text = await response.text();
    if (text) {
      try {
        payload = JSON.parse(text);
      } catch (err) {
        payload = null;
      }
    }

    if (!response.ok) {
      const message = (payload && payload.error) || 'HTTP ' + response.status;
      const error = new Error(message);
      error.status = response.status;
      error.field = payload && payload.field;
      error.payload = payload;
      throw error;
    }
    return payload;
  }

  /** 把后端错误翻译成给用户看的句子。 */
  function describeError(err) {
    if (!err) return '未知错误';
    if (err.status === 400) {
      return err.field ? '参数有误（' + err.field + '）：' + err.message : '参数有误：' + err.message;
    }
    if (err.status === 404) return '商品不存在或已被删除';
    if (err.status === 503) return '存储服务不可用，请确认 mredis-server 已启动';
    return err.message || String(err);
  }

  function setConn(ok, text) {
    const dot = $('#conn-dot');
    const label = $('#conn-text');
    if (!dot || !label) return;
    dot.classList.toggle('ok', !!ok);
    dot.classList.toggle('bad', !ok);
    label.textContent = text;
  }

  /* ------------------------------------------------------------------ *
   * 视图切换
   * ------------------------------------------------------------------ */
  function switchView(name, opts) {
    if (!VIEWS.includes(name)) name = 'market';
    state.view = name;
    $$('.tab').forEach((tab) => tab.classList.toggle('active', tab.dataset.view === name));
    $$('.view').forEach((view) => view.classList.toggle('active', view.id === 'view-' + name));

    // 深链接：地址栏带 #market / #detail / #recommend / #demo，刷新和后退都能回到同一视图
    if (!(opts && opts.silent) && location.hash !== '#' + name) {
      history.replaceState(null, '', '#' + name);
    }

    if (name === 'recommend') {
      loadUsers().then(() => loadRecommend());
    } else if (name === 'demo') {
      refreshStats();
      if (!state.benchDone) runBench();
      startPolling();
    } else {
      stopPolling();
    }
    if (!(opts && opts.silent)) window.scrollTo({ top: 0, behavior: 'smooth' });
  }

  function startPolling() {
    stopPolling();
    state.pollTimer = setInterval(() => {
      if (state.view === 'demo') refreshStats();
    }, POLL_MS);
  }

  function stopPolling() {
    if (state.pollTimer) {
      clearInterval(state.pollTimer);
      state.pollTimer = null;
    }
  }

  /* ------------------------------------------------------------------ *
   * 视图 1：市场
   * ------------------------------------------------------------------ */
  function buildQuery() {
    const params = new URLSearchParams();
    params.set('sort', state.filters.sort);
    params.set('limit', String(PAGE_SIZE));
    params.set('offset', String(state.offset));
    if (state.filters.tags.length) params.set('tags', state.filters.tags.join(','));
    if (state.filters.q) params.set('q', state.filters.q);
    if (state.filters.min !== '') params.set('min', state.filters.min);
    if (state.filters.max !== '') params.set('max', state.filters.max);
    return params.toString();
  }

  async function loadProducts() {
    const grid = $('#product-grid');
    grid.innerHTML = '';
    for (let i = 0; i < 6; i += 1) grid.appendChild(el('div', { class: 'skeleton' }));

    try {
      const data = await api('/api/products?' + buildQuery());
      state.products = data.items || [];
      state.total = data.total || 0;
      renderProducts();
      renderPager();
      setConn(true, '已连接 · ' + fmtNumber(state.total) + ' 件商品');
    } catch (err) {
      grid.innerHTML = '';
      grid.appendChild(emptyState('加载失败', describeError(err)));
      setConn(false, '连接失败');
      toast(describeError(err), 'err');
    }
  }

  function emptyState(title, detail) {
    return el('div', { class: 'empty' }, [
      el('strong', { text: title }),
      el('span', { text: detail || '' }),
    ]);
  }

  function renderProducts() {
    const grid = $('#product-grid');
    grid.innerHTML = '';
    const count = $('#market-count');
    if (count) {
      const from = state.total === 0 ? 0 : state.offset + 1;
      const to = Math.min(state.offset + state.products.length, state.total);
      count.textContent = '共 ' + fmtNumber(state.total) + ' 件 · 显示 ' + from + '-' + to;
    }

    if (!state.products.length) {
      grid.appendChild(emptyState('没有匹配的商品', '试着放宽价格区间或减少标签筛选'));
      return;
    }
    state.products.forEach((product) => grid.appendChild(productCard(product)));
  }

  function productCard(product) {
    const tags = (product.tags || []).map((tag) =>
      el('span', { class: 'chip', text: tag })
    );
    const hotScore = Number(product.hot) || 0;
    return el(
      'article',
      {
        class: 'card',
        onclick: () => openDetail(product.id),
        role: 'button',
        tabindex: '0',
        onkeydown: (event) => {
          if (event.key === 'Enter' || event.key === ' ') {
            event.preventDefault();
            openDetail(product.id);
          }
        },
      },
      [
        el('div', { class: 'card-top' }, [
          el('div', { class: 'card-title', text: product.title || '(无标题)' }),
          el('div', { class: 'card-price' }, [
            el('small', { text: '¥' }),
            document.createTextNode(yuan(product.price_cents)),
          ]),
        ]),
        el('div', { class: 'card-desc', text: product.desc || '卖家没有填写描述。' }),
        el('div', { class: 'card-tags' }, tags.concat([
          el('span', { class: 'chip dim', text: product.campus || '未知校区' }),
          el('span', { class: 'chip dim', text: '成色 ' + (product.condition || '?') + '/10' }),
          product.status && product.status !== 'onsale'
            ? el('span', { class: 'chip hot', text: statusLabel(product.status) })
            : null,
          hotScore > 0
            ? el('span', { class: 'chip hot', text: '热度 ' + hotScore.toFixed(2) })
            : null,
        ].filter(Boolean))),
        el('div', { class: 'card-meta' }, [
          el('span', { text: '👁 ' + fmtNumber(product.views) }),
          el('span', { text: '👆 ' + fmtNumber(product.clicks) }),
          el('span', { text: '⭐ ' + fmtNumber(product.favs) }),
          el('span', { text: timeAgo(product.created_at) }),
        ]),
      ]
    );
  }

  function renderPager() {
    const pager = $('#pager');
    pager.innerHTML = '';
    const pages = Math.max(1, Math.ceil(state.total / PAGE_SIZE));
    const current = Math.floor(state.offset / PAGE_SIZE) + 1;

    const prev = el('button', { class: 'btn ghost', text: '← 上一页' });
    prev.disabled = state.offset <= 0;
    prev.addEventListener('click', () => {
      state.offset = Math.max(0, state.offset - PAGE_SIZE);
      loadProducts();
    });

    const next = el('button', { class: 'btn ghost', text: '下一页 →' });
    next.disabled = state.offset + PAGE_SIZE >= state.total;
    next.addEventListener('click', () => {
      state.offset += PAGE_SIZE;
      loadProducts();
    });

    pager.appendChild(prev);
    pager.appendChild(el('span', { class: 'page-info', text: current + ' / ' + pages }));
    pager.appendChild(next);
  }

  function renderFilterTags() {
    const box = $('#f-tags');
    if (!box) return;
    box.innerHTML = '';
    if (!state.tags.length) {
      box.appendChild(el('span', { class: 'muted small', text: '（还没有标签数据）' }));
      return;
    }
    state.tags.forEach((tag) => {
      const input = el('input', { type: 'checkbox', value: tag });
      const label = el('label', { class: 'tag-check' }, [input, document.createTextNode(tag)]);
      input.addEventListener('change', () => {
        label.classList.toggle('on', input.checked);
      });
      box.appendChild(label);
    });
  }

  /** 从商品列表里收集全部标签（后端没有专门的标签接口）。 */
  function collectTags(products) {
    const seen = new Set();
    products.forEach((product) => {
      (product.tags || []).forEach((tag) => seen.add(tag));
    });
    return Array.from(seen).sort((a, b) => a.localeCompare(b, 'zh-CN'));
  }

  async function loadTagPool() {
    try {
      const data = await api('/api/products?limit=200&sort=hot_desc');
      state.tags = collectTags(data.items || []);
      renderFilterTags();
    } catch (err) {
      state.tags = [];
      renderFilterTags();
    }
  }

  function applyFilters() {
    state.filters.sort = $('#f-sort').value;
    state.filters.q = $('#f-q').value.trim();
    state.filters.min = $('#f-min').value.trim();
    state.filters.max = $('#f-max').value.trim();
    state.filters.tags = $$('#f-tags input:checked').map((input) => input.value);
    state.offset = 0;
    loadProducts();
  }

  function resetFilters() {
    $('#f-sort').value = 'time_desc';
    $('#f-q').value = '';
    $('#f-min').value = '';
    $('#f-max').value = '';
    $$('#f-tags input').forEach((input) => {
      input.checked = false;
      input.closest('.tag-check').classList.remove('on');
    });
    state.filters = { sort: 'time_desc', q: '', min: '', max: '', tags: [] };
    state.offset = 0;
    loadProducts();
  }

  /* ------------------------------------------------------------------ *
   * 视图 2：详情 / 发布
   * ------------------------------------------------------------------ */
  async function openDetail(id) {
    switchView('detail', { silent: true });
    // 深链接：#detail/<id> 可以直接分享某个商品的地址，刷新后仍停在该商品上
    const want = '#detail/' + id;
    if (location.hash !== want) history.replaceState(null, '', want);
    const panel = $('#detail-panel');
    panel.innerHTML = '';
    panel.appendChild(el('h2', { text: '商品详情' }));
    panel.appendChild(el('p', { class: 'muted', text: '加载中…' }));

    try {
      const product = await api('/api/products/' + encodeURIComponent(id));
      state.currentProduct = product;
      renderDetail(product);
    } catch (err) {
      panel.innerHTML = '';
      panel.appendChild(el('h2', { text: '商品详情' }));
      panel.appendChild(emptyState('无法加载商品', describeError(err)));
      toast(describeError(err), 'err');
    }
  }

  function renderDetail(product) {
    const panel = $('#detail-panel');
    panel.innerHTML = '';

    const tags = (product.tags || []).map((tag) => el('span', { class: 'chip', text: tag }));

    panel.appendChild(
      el('div', { class: 'detail-head' }, [
        el('div', { class: 'detail-title', text: product.title || '(无标题)' }),
        el('div', { class: 'detail-price' }, [
          el('small', { text: '¥' }),
          document.createTextNode(yuan(product.price_cents)),
        ]),
      ])
    );

    panel.appendChild(el('div', { class: 'card-tags' }, tags.concat([
      el('span', { class: 'chip dim', text: product.campus || '未知校区' }),
      el('span', { class: 'chip dim', text: '成色 ' + (product.condition || '?') + '/10' }),
      el('span', { class: 'chip dim', text: statusLabel(product.status) }),
    ])));

    panel.appendChild(el('div', { class: 'detail-desc', text: product.desc || '卖家没有填写描述。' }));

    const kv = el('dl', { class: 'kv' }, []);
    [
      ['商品 ID', '#' + product.id],
      ['卖家', '用户 ' + product.seller_id],
      ['发布时间', timeAgo(product.created_at)],
      ['浏览量', fmtNumber(product.views)],
      ['点击数', fmtNumber(product.clicks)],
      ['收藏数', fmtNumber(product.favs)],
      ['热度分', (Number(product.hot) || 0).toFixed(4)],
      ['标签数', String((product.tags || []).length)],
    ].forEach(([label, value]) => {
      kv.appendChild(el('div', {}, [el('dt', { text: label }), el('dd', { text: value })]));
    });
    panel.appendChild(kv);

    const bar = el('div', { class: 'event-bar' });
    [
      ['view', '浏览'],
      ['click', '想要'],
      ['fav', '收藏'],
    ].forEach(([kind, label]) => {
      const button = el('button', { class: 'btn', text: label });
      button.addEventListener('click', () => sendEvent(product.id, kind, button));
      bar.appendChild(button);
    });

    const similar = el('button', { class: 'btn ghost', text: '看相似商品' });
    similar.addEventListener('click', () => loadSimilar(product.id));
    bar.appendChild(similar);
    panel.appendChild(bar);

    panel.appendChild(renderOwnerBar(product));

    panel.appendChild(el('div', { id: 'similar-box' }));
  }

  async function sendEvent(productId, kind, button) {
    const userId = Number($('#rec-user') && $('#rec-user').value) || 1;
    const original = button.textContent;
    button.disabled = true;
    button.textContent = '提交中…';
    try {
      const data = await api('/api/products/' + productId + '/event', {
        method: 'POST',
        body: { user_id: userId, kind: kind },
      });
      if (data && data.product) renderDetail(data.product);
      toast('已记录「' + ({ view: '浏览', click: '想要', fav: '收藏' }[kind] || kind) + '」，热度已更新', 'ok');
    } catch (err) {
      button.disabled = false;
      button.textContent = original;
      toast(describeError(err), 'err');
    }
  }

  /* ------------------------------------------------------------------ *
   * 卖家操作：编辑 / 上下架 / 删除
   * ------------------------------------------------------------------ */
  function renderOwnerBar(product) {
    const wrap = el('div', { class: 'owner-box' }, [
      el('h3', { text: '管理这件商品' }),
      el('p', {
        class: 'muted small',
        text: '修改信息、标记售出或删除都会立即写入 AOF，重启服务后依然生效。',
      }),
    ]);

    const bar = el('div', { class: 'owner-bar' });

    const edit = el('button', { class: 'btn', text: '编辑信息' });
    edit.addEventListener('click', () => renderEditForm(product));
    bar.appendChild(edit);

    const sold = product.status === 'sold';
    const toggle = el('button', { class: 'btn ghost', text: sold ? '恢复在售' : '标记已售出' });
    toggle.addEventListener('click', () => patchStatus(product, sold ? 'onsale' : 'sold', toggle));
    bar.appendChild(toggle);

    const del = el('button', { class: 'btn danger', text: '删除商品' });
    del.addEventListener('click', () => removeProduct(product, del));
    bar.appendChild(del);

    wrap.appendChild(bar);
    return wrap;
  }

  async function patchStatus(product, status, button) {
    const original = button.textContent;
    button.disabled = true;
    button.textContent = '保存中…';
    try {
      const data = await api('/api/products/' + product.id, {
        method: 'PATCH',
        body: { status: status },
      });
      toast('已更新为「' + statusLabel(status) + '」', 'ok');
      if (data && data.product) renderDetail(data.product);
      loadProducts();
    } catch (err) {
      button.disabled = false;
      button.textContent = original;
      toast(describeError(err), 'err');
    }
  }

  async function removeProduct(product, button) {
    if (!window.confirm('确定删除商品「' + (product.title || '#' + product.id) + '」吗？这一操作不可撤销。')) {
      return;
    }
    const original = button.textContent;
    button.disabled = true;
    button.textContent = '删除中…';
    try {
      await api('/api/products/' + product.id, { method: 'DELETE' });
      toast('商品 #' + product.id + ' 已删除', 'ok');
      state.currentProduct = null;
      switchView('market');
      loadProducts();
      loadTagPool();
    } catch (err) {
      button.disabled = false;
      button.textContent = original;
      toast(describeError(err), 'err');
    }
  }

  /** 就地编辑表单：保存后 PATCH 回后端，再刷新详情。 */
  function renderEditForm(product) {
    const panel = $('#detail-panel');
    panel.innerHTML = '';
    panel.appendChild(el('h2', { text: '编辑商品 #' + product.id }));

    const form = el('form', { class: 'form', autocomplete: 'off' });

    const titleInput = el('input', {
      name: 'title',
      type: 'text',
      maxlength: '80',
      value: product.title || '',
      required: 'required',
    });
    form.appendChild(el('label', { class: 'field' }, [el('span', { text: '标题' }), titleInput]));

    const descInput = el('textarea', { name: 'desc', rows: '3', maxlength: '500' });
    descInput.value = product.desc || '';
    form.appendChild(el('label', { class: 'field' }, [el('span', { text: '描述' }), descInput]));

    const priceInput = el('input', {
      name: 'price',
      type: 'number',
      min: '0.01',
      step: '0.01',
      value: yuan(product.price_cents),
      required: 'required',
    });
    const condInput = el('input', {
      name: 'condition',
      type: 'number',
      min: '1',
      max: '10',
      step: '1',
      value: String(product.condition || 9),
    });
    form.appendChild(
      el('div', { class: 'form-row' }, [
        el('label', { class: 'field' }, [el('span', { text: '价格（元）' }), priceInput]),
        el('label', { class: 'field' }, [el('span', { text: '成色（1-10）' }), condInput]),
      ])
    );

    const sellerSelect = el('select', { name: 'seller_id' });
    const users = state.users.length
      ? state.users.slice()
      : [{ id: product.seller_id, name: '用户 ' + product.seller_id }];
    if (!users.some((user) => user.id === product.seller_id)) {
      users.unshift({ id: product.seller_id, name: '用户 ' + product.seller_id });
    }
    users.forEach((user) => {
      sellerSelect.appendChild(el('option', { value: String(user.id), text: '#' + user.id + ' ' + user.name }));
    });
    sellerSelect.value = String(product.seller_id);

    const campusInput = el('input', {
      name: 'campus',
      type: 'text',
      maxlength: '20',
      value: product.campus || '',
    });
    form.appendChild(
      el('div', { class: 'form-row' }, [
        el('label', { class: 'field' }, [el('span', { text: '卖家' }), sellerSelect]),
        el('label', { class: 'field' }, [el('span', { text: '校区' }), campusInput]),
      ])
    );

    const tagsInput = el('input', {
      name: 'tags',
      type: 'text',
      value: (product.tags || []).join(', '),
      required: 'required',
    });
    form.appendChild(
      el('label', { class: 'field' }, [el('span', { text: '标签（逗号分隔）' }), tagsInput])
    );

    const statusSelect = el('select', { name: 'status' });
    [
      ['onsale', '在售'],
      ['reserved', '已预订'],
      ['sold', '已售出'],
    ].forEach(([value, label]) => {
      statusSelect.appendChild(el('option', { value: value, text: label }));
    });
    statusSelect.value = ['onsale', 'reserved', 'sold'].includes(product.status)
      ? product.status
      : 'onsale';
    form.appendChild(el('label', { class: 'field' }, [el('span', { text: '状态' }), statusSelect]));

    const save = el('button', { class: 'btn primary', type: 'submit', text: '保存修改' });
    const cancel = el('button', { class: 'btn ghost', type: 'button', text: '取消' });
    cancel.addEventListener('click', () => renderDetail(product));
    form.appendChild(el('div', { class: 'form-actions' }, [save, cancel]));

    form.addEventListener('submit', async (event) => {
      event.preventDefault();
      const priceCents = toCents(priceInput.value);
      if (!isFinite(priceCents) || priceCents <= 0) {
        toast('价格必须大于 0 元', 'err');
        return;
      }
      const tags = String(tagsInput.value || '')
        .split(/[,，]/)
        .map((tag) => tag.trim())
        .filter(Boolean);
      if (!tags.length) {
        toast('请至少填写一个标签', 'err');
        return;
      }
      const body = {
        title: titleInput.value.trim(),
        desc: descInput.value.trim(),
        price_cents: priceCents,
        condition: Number(condInput.value) || 9,
        seller_id: Number(sellerSelect.value) || product.seller_id,
        campus: campusInput.value.trim(),
        tags: tags,
        status: statusSelect.value,
      };
      save.disabled = true;
      const original = save.textContent;
      save.textContent = '保存中…';
      try {
        const data = await api('/api/products/' + product.id, { method: 'PATCH', body: body });
        toast('已保存商品 #' + product.id, 'ok');
        const updated = (data && data.product) || Object.assign({}, product, body);
        state.currentProduct = updated;
        renderDetail(updated);
        loadProducts();
        loadTagPool();
      } catch (err) {
        toast(describeError(err), 'err');
        save.disabled = false;
        save.textContent = original;
      }
    });

    panel.appendChild(form);
  }

  async function loadSimilar(productId) {
    const box = $('#similar-box');
    if (!box) return;
    box.innerHTML = '';
    box.appendChild(el('h3', { text: '相似商品' }));
    const wrap = el('div', { class: 'grid', style: 'margin-top:10px' });
    box.appendChild(wrap);

    try {
      const data = await api('/api/similar/' + productId + '?limit=3');
      const items = data.items || [];
      if (!items.length) {
        wrap.appendChild(emptyState('暂无相似商品', '共现数据还不够，多浏览几件商品试试'));
        return;
      }
      items.forEach((item) => {
        if (!item.product) return;
        const card = productCard(item.product);
        const reasonLine = el('div', { class: 'reasons' }, []);
        (item.reasons || []).forEach((reason) => {
          reasonLine.appendChild(reasonRow(reason));
        });
        card.appendChild(reasonLine);
        wrap.appendChild(card);
      });
    } catch (err) {
      wrap.appendChild(emptyState('无法加载相似商品', describeError(err)));
    }
  }

  async function submitPublish(event) {
    event.preventDefault();
    const form = event.target;
    const submit = form.querySelector('button[type="submit"]');
    const data = new FormData(form);

    const priceCents = toCents(data.get('price'));
    if (!isFinite(priceCents) || priceCents <= 0) {
      toast('价格必须大于 0 元', 'err');
      return;
    }
    const tags = String(data.get('tags') || '')
      .split(/[,，]/)
      .map((tag) => tag.trim())
      .filter(Boolean);
    if (!tags.length) {
      toast('请至少填写一个标签', 'err');
      return;
    }

    const body = {
      title: String(data.get('title') || '').trim(),
      desc: String(data.get('desc') || '').trim(),
      price_cents: priceCents,
      tags: tags,
      seller_id: Number(data.get('seller_id')) || 1,
      condition: Number(data.get('condition')) || 9,
      campus: String(data.get('campus') || '前湖校区').trim(),
    };

    submit.disabled = true;
    const original = submit.textContent;
    submit.textContent = '发布中…';
    try {
      const created = await api('/api/products', { method: 'POST', body: body });
      toast('发布成功，商品 ID #' + created.id, 'ok');
      form.reset();
      if ($('#p-seller')) $('#p-seller').value = String(body.seller_id);
      loadTagPool();
      await loadProducts();
      openDetail(created.id);
    } catch (err) {
      toast(describeError(err), 'err');
    } finally {
      submit.disabled = false;
      submit.textContent = original;
    }
  }

  /* ------------------------------------------------------------------ *
   * 视图 3：为你推荐
   * ------------------------------------------------------------------ */
  async function loadUsers() {
    if (state.users.length) return state.users;
    try {
      const data = await api('/api/users');
      state.users = data.items || [];
      fillUserSelects();
      return state.users;
    } catch (err) {
      toast('无法加载用户列表：' + describeError(err), 'err');
      return [];
    }
  }

  function fillUserSelects() {
    const recSelect = $('#rec-user');
    const sellerSelect = $('#p-seller');
    if (recSelect) {
      const previous = recSelect.value;
      recSelect.innerHTML = '';
      state.users.forEach((user) => {
        const interests = (user.interests || []).join('、') || '无兴趣标签（冷启动）';
        recSelect.appendChild(
          el('option', {
            value: String(user.id),
            text: '#' + user.id + ' ' + user.name + ' · ' + (user.campus || '—') + ' · ' + interests,
          })
        );
      });
      if (previous) recSelect.value = previous;
    }
    if (sellerSelect) {
      const previous = sellerSelect.value;
      sellerSelect.innerHTML = '';
      state.users.forEach((user) => {
        sellerSelect.appendChild(
          el('option', { value: String(user.id), text: '#' + user.id + ' ' + user.name })
        );
      });
      if (previous) sellerSelect.value = previous;
    }
  }

  async function loadRecommend() {
    const grid = $('#rec-grid');
    const note = $('#rec-note');
    if (!grid) return;
    const userId = Number($('#rec-user').value) || 1;
    const limit = Number($('#rec-limit').value) || 10;
    // 连点「刷新」或快速切用户时，只让最后一次请求的结果落到 DOM
    const seq = (state.recSeq = (state.recSeq || 0) + 1);

    grid.innerHTML = '';
    for (let i = 0; i < 4; i += 1) grid.appendChild(el('div', { class: 'skeleton' }));
    if (note) note.textContent = '正在为用户 #' + userId + ' 计算推荐…';

    try {
      const data = await api('/api/recommend?user_id=' + userId + '&limit=' + limit);
      if (seq !== state.recSeq) return;
      const items = data.items || [];
      grid.innerHTML = '';
      if (!items.length) {
        grid.appendChild(emptyState('暂无推荐结果', '先导入种子数据，或在市场里产生一些浏览行为'));
        if (note) note.textContent = '';
        return;
      }
      const user = state.users.find((u) => u.id === userId);
      if (note) {
        const coldStart = items.every((item) =>
          (item.reasons || []).some((reason) => reason.type === 'cold_start')
        );
        note.textContent =
          '用户 #' + userId + (user ? ' ' + user.name : '') +
          ' · 返回 ' + items.length + ' 条' +
          (coldStart ? ' · 该用户没有兴趣标签，走冷启动热门兜底' : '');
      }
      items.forEach((item) => grid.appendChild(recommendCard(item)));
    } catch (err) {
      if (seq !== state.recSeq) return;
      grid.innerHTML = '';
      grid.appendChild(emptyState('推荐加载失败', describeError(err)));
      if (note) note.textContent = '';
      toast(describeError(err), 'err');
    }
  }

  function recommendCard(item) {
    const product = item.product || {};
    const score = Math.max(0, Math.min(1, Number(item.score) || 0));
    const card = el('article', { class: 'card' }, []);
    if (product.id) {
      card.addEventListener('click', () => openDetail(product.id));
      card.style.cursor = 'pointer';
    }

    card.appendChild(
      el('div', { class: 'card-top' }, [
        el('div', { class: 'card-title', text: product.title || ('商品 #' + item.product_id) }),
        el('div', { class: 'card-price' }, [
          el('small', { text: '¥' }),
          document.createTextNode(yuan(product.price_cents)),
        ]),
      ])
    );

    card.appendChild(
      el('div', { class: 'rec-score' }, [
        el('span', { text: 'score ' + score.toFixed(3) }),
        el('span', { class: 'score-bar' }, [el('i', { style: 'width:' + Math.round(score * 100) + '%' })]),
      ])
    );

    if (product.desc) {
      card.appendChild(el('div', { class: 'card-desc', text: product.desc }));
    }

    const reasons = el('div', { class: 'reasons' }, []);
    (item.reasons || []).forEach((reason) => reasons.appendChild(reasonRow(reason)));
    if (!(item.reasons || []).length) {
      reasons.appendChild(el('div', { class: 'reason' }, [
        el('span', { class: 'bullet' }),
        el('span', { text: '综合评分推荐' }),
      ]));
    }
    card.appendChild(reasons);

    if (product.tags && product.tags.length) {
      card.appendChild(
        el('div', { class: 'card-tags' }, product.tags.slice(0, 4).map((tag) =>
          el('span', { class: 'chip', text: tag })
        ))
      );
    }
    return card;
  }

  /** 把 REC.* 的 reason 对象翻译成中文说明。 */
  function reasonRow(reason) {
    if (!reason || typeof reason !== 'object') {
      return el('div', { class: 'reason' }, [
        el('span', { class: 'bullet' }),
        el('span', { text: String(reason) }),
      ]);
    }
    const type = String(reason.type || '');
    let text = '综合推荐';
    let cls = 'reason';

    if (type === 'tag_match') {
      const tags = Array.isArray(reason.tags) ? reason.tags : [];
      text = tags.length ? '标签匹配：' + tags.join('、') : '标签与你的兴趣匹配';
    } else if (type === 'similar_users') {
      const users = Array.isArray(reason.users) ? reason.users : [];
      const count = users.length || Number(reason.count) || 0;
      text = count > 0 ? '相似用户 ' + count + ' 人也看过' : '相似用户也看过';
      cls += ' cf';
    } else if (type === 'trending') {
      const factor = Number(reason.factor);
      text = '近期热度高' + (isFinite(factor) ? '（系数 ' + factor.toFixed(2) + '）' : '');
      cls += ' hot';
    } else if (type === 'price_fit') {
      text = '价格符合你的预算';
      cls += ' price';
    } else if (type === 'campus_match') {
      text = '同校区 · 可当面交易';
      cls += ' price';
    } else if (type === 'cold_start') {
      text = '冷启动 · 热门推荐';
      cls += ' cold';
    } else if (reason.text) {
      text = String(reason.text);
    } else if (type) {
      text = '推荐理由：' + type;
    }

    return el('div', { class: cls }, [el('span', { class: 'bullet' }), el('span', { text: text })]);
  }

  /* ------------------------------------------------------------------ *
   * 视图 4：演示面板
   * ------------------------------------------------------------------ */
  async function refreshStats() {
    try {
      const stats = await api('/api/stats');
      renderStats(stats);
      setConn(true, '已连接 · ' + fmtNumber(stats.dbsize) + ' 个键');
    } catch (err) {
      setConn(false, '连接失败');
      toast(describeError(err), 'err');
    }
  }

  function renderStats(stats) {
    const dict = stats.dict || {};
    drawDistribution(dict.distribution || [], dict.buckets || 0, dict.rehashing);

    const dictStats = $('#dict-stats');
    if (dictStats) {
      dictStats.innerHTML = '';
      [
        ['桶数量', fmtNumber(dict.buckets), 'accent'],
        ['扩容中', dict.rehashing ? '是' : '否', dict.rehashing ? 'amber' : 'green'],
        ['键总数', fmtNumber(stats.dbsize), ''],
        ['非空桶', String((dict.distribution || []).filter((n) => n > 0).length), 'cyan'],
        ['最长链', String(Math.max.apply(null, [0].concat(dict.distribution || []))), 'purple'],
      ].forEach(([label, value, tone]) => {
        dictStats.appendChild(
          el('div', { class: 'stat' }, [
            el('div', { class: 'label', text: label }),
            el('div', { class: 'value ' + (tone || ''), text: value }),
          ])
        );
      });
    }

    const recCards = $('#rec-stats-cards');
    if (recCards) {
      const rec = stats.rec || {};
      recCards.innerHTML = '';
      [
        ['商品', rec.products, 'accent'],
        ['用户', rec.users, 'green'],
        ['标签', rec.tags, 'cyan'],
        ['共现对', rec.co_occurrence_pairs, 'purple'],
        ['事件', rec.events, 'amber'],
      ].forEach(([label, value, tone]) => {
        recCards.appendChild(
          el('div', { class: 'stat' }, [
            el('div', { class: 'label', text: label }),
            el('div', { class: 'value ' + (tone || ''), text: fmtNumber(value) }),
          ])
        );
      });
    }

    const coreStats = $('#core-stats');
    if (coreStats) {
      const aof = stats.aof || {};
      coreStats.innerHTML = '';
      [
        ['命中率', ((Number(stats.hit_rate) || 0) * 100).toFixed(2) + '%', 'green'],
        ['命令总数', fmtNumber(stats.total_commands), 'accent'],
        ['运行时长', Math.floor((Number(stats.uptime_secs) || 0) / 60) + ' 分', ''],
        ['AOF', aof.enabled ? '开启' : '关闭', aof.enabled ? 'green' : ''],
        ['AOF 命令数', fmtNumber(aof.commands), 'cyan'],
        ['AOF 大小', fmtNumber(aof.size) + ' B', 'purple'],
      ].forEach(([label, value, tone]) => {
        coreStats.appendChild(
          el('div', { class: 'stat' }, [
            el('div', { class: 'label', text: label }),
            el('div', { class: 'value ' + (tone || ''), text: value }),
          ])
        );
      });
    }

    renderCommandStream(stats.recent_commands || []);
  }

  function renderCommandStream(commands) {
    const box = $('#cmd-stream');
    if (!box) return;
    box.innerHTML = '';
    if (!commands.length) {
      box.appendChild(el('div', { class: 'muted small', text: '暂无命令记录。' }));
      return;
    }
    commands.slice(0, 50).forEach((line, index) => {
      const text = String(line);
      const space = text.indexOf(' ');
      const head = space > 0 ? text.slice(0, space) : text;
      const rest = space > 0 ? text.slice(space) : '';
      box.appendChild(
        el('div', { class: 'cmd-line' + (index === 0 ? ' new' : '') }, [
          el('span', { class: 'seq', text: String(index + 1) }),
          el('span', { class: 'cmd', html: '<b>' + escapeHtml(head) + '</b>' + escapeHtml(rest) }),
        ])
      );
    });
  }

  /** 手绘哈希桶分布柱状图（canvas 2D，零依赖）。 */
  function drawDistribution(distribution, buckets, rehashing) {
    const canvas = $('#dict-canvas');
    if (!canvas) return;
    const ctx = canvas.getContext('2d');
    if (!ctx) return;

    const dpr = window.devicePixelRatio || 1;
    const cssWidth = canvas.clientWidth || 640;
    const cssHeight = 220;
    canvas.width = Math.round(cssWidth * dpr);
    canvas.height = Math.round(cssHeight * dpr);
    ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
    ctx.clearRect(0, 0, cssWidth, cssHeight);

    const padLeft = 34;
    const padRight = 8;
    const padTop = 12;
    const padBottom = 22;
    const plotW = cssWidth - padLeft - padRight;
    const plotH = cssHeight - padTop - padBottom;

    const values = distribution.length ? distribution : [0];
    const max = Math.max.apply(null, [1].concat(values));
    const yTicks = 4;

    // 网格与纵轴刻度
    ctx.strokeStyle = 'rgba(42, 51, 65, 0.9)';
    ctx.fillStyle = '#64748b';
    ctx.font = '10px ui-monospace, Consolas, monospace';
    ctx.lineWidth = 1;
    ctx.textAlign = 'right';
    ctx.textBaseline = 'middle';
    for (let i = 0; i <= yTicks; i += 1) {
      const value = Math.round((max / yTicks) * i);
      const y = padTop + plotH - (plotH * i) / yTicks;
      ctx.beginPath();
      ctx.moveTo(padLeft, y + 0.5);
      ctx.lineTo(padLeft + plotW, y + 0.5);
      ctx.stroke();
      ctx.fillText(String(value), padLeft - 6, y);
    }

    // 柱子
    const slot = plotW / values.length;
    const barW = Math.max(1, Math.min(18, slot * 0.68));
    values.forEach((value, index) => {
      const ratio = max > 0 ? value / max : 0;
      const height = Math.max(value > 0 ? 2 : 0, ratio * plotH);
      const x = padLeft + slot * index + (slot - barW) / 2;
      const y = padTop + plotH - height;

      const gradient = ctx.createLinearGradient(0, y, 0, padTop + plotH);
      gradient.addColorStop(0, rehashing ? '#d29922' : '#4f9cf9');
      gradient.addColorStop(1, rehashing ? 'rgba(210,153,34,0.25)' : 'rgba(79,156,249,0.22)');
      ctx.fillStyle = gradient;
      ctx.beginPath();
      const radius = Math.min(3, barW / 2);
      roundRect(ctx, x, y, barW, height, radius);
      ctx.fill();
    });

    // 横轴标签（只标首尾和中间，避免拥挤）
    ctx.fillStyle = '#64748b';
    ctx.textAlign = 'center';
    ctx.textBaseline = 'top';
    const labels = [0, Math.floor(values.length / 2), values.length - 1];
    labels.forEach((index) => {
      if (index < 0 || index >= values.length) return;
      const x = padLeft + slot * index + slot / 2;
      ctx.fillText(String(index), x, padTop + plotH + 6);
    });
    ctx.textAlign = 'left';
    ctx.fillText('桶序号 →', padLeft + plotW - 54, padTop + plotH + 6);

    if (!distribution.length) {
      ctx.fillStyle = '#64748b';
      ctx.textAlign = 'center';
      ctx.textBaseline = 'middle';
      ctx.font = '12px system-ui, sans-serif';
      ctx.fillText('等待 INFO 的 dict_distribution 数据…', padLeft + plotW / 2, padTop + plotH / 2);
    }
  }

  function roundRect(ctx, x, y, width, height, radius) {
    const r = Math.max(0, Math.min(radius, width / 2, height / 2));
    ctx.moveTo(x + r, y);
    ctx.lineTo(x + width - r, y);
    ctx.quadraticCurveTo(x + width, y, x + width, y + r);
    ctx.lineTo(x + width, y + height);
    ctx.lineTo(x, y + height);
    ctx.lineTo(x, y + r);
    ctx.quadraticCurveTo(x, y, x + r, y);
  }

  async function runBench() {
    const n = Number($('#bench-n').value) || 20000;
    const button = $('#bench-run');
    const tbody = $('#bench-table tbody');
    button.disabled = true;
    button.textContent = '测量中…';
    tbody.innerHTML = '';
    tbody.appendChild(
      el('tr', {}, [el('td', { colspan: '4', class: 'muted', text: '正在跑 n = ' + fmtNumber(n) + ' …' })])
    );

    try {
      const data = await api('/api/bench/sort?n=' + n);
      tbody.innerHTML = '';
      (data.results || []).forEach((row) => {
        tbody.appendChild(
          el('tr', {}, [
            el('td', { text: row.label || row.algorithm }),
            el('td', { text: fmtMs(row.ms) }),
            el('td', { text: row.comparisons === null || row.comparisons === undefined ? '—' : fmtNumber(row.comparisons) }),
            el('td', { text: row.complexity || '—' }),
          ])
        );
      });
      const note = $('#bench-note');
      if (note) {
        note.textContent =
          'n = ' + fmtNumber(data.n) + '，Top-K 的 k = ' + fmtNumber(data.k) +
          '，三路一致性校验：' + (data.consistent ? '通过' : '不一致（请检查）');
      }
      state.benchDone = true;
    } catch (err) {
      tbody.innerHTML = '';
      tbody.appendChild(el('tr', {}, [el('td', { colspan: '4', class: 'muted', text: describeError(err) })]));
      toast(describeError(err), 'err');
    } finally {
      button.disabled = false;
      button.textContent = '重新测量';
    }
  }

  /* ------------------------------------------------------------------ *
   * 健康检查
   * ------------------------------------------------------------------ */
  async function checkHealth() {
    try {
      const data = await api('/api/health');
      setConn(!!data.ok, '已连接 · ' + fmtNumber(data.dbsize) + ' 个键');
      return true;
    } catch (err) {
      setConn(false, err.status === 503 ? '存储不可用' : '连接失败');
      return false;
    }
  }

  /* ------------------------------------------------------------------ *
   * 事件绑定与启动
   * ------------------------------------------------------------------ */
  function bind() {
    $$('.tab').forEach((tab) => {
      tab.addEventListener('click', () => switchView(tab.dataset.view));
    });

    $('#f-apply').addEventListener('click', applyFilters);
    $('#f-reset').addEventListener('click', resetFilters);
    $('#f-sort').addEventListener('change', applyFilters);
    $('#f-q').addEventListener('keydown', (event) => {
      if (event.key === 'Enter') {
        event.preventDefault();
        applyFilters();
      }
    });

    $('#publish-form').addEventListener('submit', submitPublish);

    $('#rec-refresh').addEventListener('click', loadRecommend);
    $('#rec-user').addEventListener('change', loadRecommend);
    $('#rec-limit').addEventListener('change', loadRecommend);

    $('#bench-run').addEventListener('click', runBench);
    $('#bench-n').addEventListener('change', runBench);

    // 窗口尺寸变化时重绘柱状图（canvas 是响应式的）
    let resizeTimer = null;
    window.addEventListener('resize', () => {
      if (state.view !== 'demo') return;
      if (resizeTimer) clearTimeout(resizeTimer);
      resizeTimer = setTimeout(() => refreshStats(), 220);
    });

    document.addEventListener('visibilitychange', () => {
      if (document.hidden) stopPolling();
      else if (state.view === 'demo') startPolling();
    });

    // 浏览器前进/后退切换视图
    window.addEventListener('hashchange', () => {
      const raw = (location.hash || '').replace('#', '');
      const name = raw.split('/')[0];
      if (!VIEWS.includes(name)) return;
      if (name === 'detail') {
        const id = raw.split('/')[1];
        if (id && (!state.currentProduct || String(state.currentProduct.id) !== id)) {
          openDetail(id);
          return;
        }
      }
      if (name !== state.view) switchView(name, { silent: true });
    });
  }

  async function boot() {
    bind();
    const healthy = await checkHealth();
    if (!healthy) {
      toast('无法连接后端，请先启动 mredis-server 与 mredis-web', 'err');
    }
    await Promise.all([loadProducts(), loadTagPool(), loadUsers()]);
    const raw = (location.hash || '').replace('#', '');
    const initial = raw.split('/')[0];
    const detailId = raw.split('/')[1];
    if (initial === 'detail' && detailId) {
      // 直接打开 #detail/12 时不必先闪一下市场视图
      await openDetail(detailId);
      return;
    }
    switchView(VIEWS.includes(initial) ? initial : 'market', { silent: true });
  }

  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', boot);
  } else {
    boot();
  }
})();
