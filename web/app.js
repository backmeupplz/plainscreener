// plainscreener: the chart is server-rendered; this adds pan/zoom/crosshair (via the
// same Rust renderer compiled to wasm), live updates, and copy buttons.
const $ = (s, el = document) => el.querySelector(s)

// Skeleton rows in the shape of the rows they stand for: one cell per column, each a bar
// of the given width ('c' for an avatar, 'q' for an icon button, a second number for a small second line, 'h' to
// hide on phones like the real column). Same cells, same heights: nothing jumps when the
// data arrives.
const skeleton = (n, cols) =>
  `<tr class="sk" aria-hidden="true">${cols
    .map((c) => {
      const [w, w2, opt] = [].concat(c)
      const bar = (x, cls = '') => (x === 'c' || x === 'q' ? `<i class="${x}"></i>` : `<i class="${cls}" style="width:${x}px"></i>`)
      return `<td class="${typeof w === 'string' && w.startsWith('l') ? 'l' : ''}${opt === 'h' ? ' hide-s' : ''}">${String(w).replace('l', '').split('+').map((x) => bar(isNaN(x) ? x : +x)).join('')}${w2 ? bar(w2, 's') : ''}</td>`
    })
    .join('')}</tr>`.repeat(n)
// Rows of a trades table, top traders, and pair lists.
const SK_TRADES = ['l84', 'l30', 54, 70, 44, [80, 0, 'h'], ['q', 0, 'h']]
const SK_TRADERS = ['l14', 'l90', [52, 64], [52, 64], [56, 40], [44, 0, 'h']]
const SK_LIST = ['l20', 'lc+120', 60, [40, 0, 'h'], [40, 0, 'h'], [40, 0, 'h'], [44, 0, 'h'], [50, 0, 'h'], [50, 0, 'h'], [50, 0, 'h'], [36, 0, 'h'], [24, 0, 'h']]

// Farcaster mini app: the host keeps the app hidden until it says it's ready. This is the
// mini app SDK's sdk.actions.ready() (one comlink call to the host) without loading the SDK.
// Elsewhere (an iframe on some site, X's player) nobody listens and it's ignored.
{
  const ready = { id: 'ready', type: 'APPLY', path: ['ready'], argumentList: [{ type: 'RAW', value: {} }] }
  if (window.ReactNativeWebView) window.ReactNativeWebView.postMessage(JSON.stringify(ready))
  else if (window.parent !== window) window.parent.postMessage(ready, '*')
}

// Copy buttons. Icon-only ones flash a check; others say "Copied". Where the clipboard is
// blocked (some frames), fall back to selecting a hidden textarea.
document.addEventListener('click', async (e) => {
  const b = e.target.closest('[data-copy]')
  if (!b) return
  try {
    await navigator.clipboard.writeText(b.dataset.copy)
  } catch {
    const t = Object.assign(document.createElement('textarea'), { value: b.dataset.copy })
    document.body.append(t)
    t.select()
    document.execCommand('copy')
    t.remove()
  }
  const html = b.innerHTML
  b.innerHTML = b.classList.contains('ic') ? '<svg class="i" aria-hidden="true"><use href="#i-check"/></svg>' : html.replace(/[^>]*$/, 'Copied')
  setTimeout(() => (b.innerHTML = html), 1200)
})

// Popover menus open under their button (inside the viewport).
document.addEventListener(
  'toggle',
  (e) => {
    const menu = e.target
    if (!menu.matches?.('[popover]') || e.newState !== 'open') return
    const b = document.querySelector(`[popovertarget="${menu.id}"]`)
    if (!b) return
    const r = b.getBoundingClientRect()
    const m = menu.getBoundingClientRect()
    menu.style.left = `${Math.max(12, Math.min(r.right - m.width, innerWidth - m.width - 12))}px`
    menu.style.top = `${r.bottom + m.height + 8 > innerHeight ? Math.max(12, r.top - m.height - 8) : r.bottom + 8}px`
  },
  true
)

// Stats windows (5m, 1h, 6h, 24h).
document.addEventListener('click', (e) => {
  const t = e.target.closest('.wt [data-w]')
  if (!t) return
  t.parentElement.querySelectorAll('[data-w]').forEach((b) => b.setAttribute('aria-selected', String(b === t)))
  document.querySelectorAll('.wp').forEach((p) => (p.hidden = p.dataset.w !== t.dataset.w))
})

// Theme: follows the system until picked here; the choice is applied before paint by
// an inline script in <head>.
$('.theme')?.addEventListener('click', () => {
  const r = document.documentElement
  const dark = (r.dataset.theme || (matchMedia('(prefers-color-scheme: dark)').matches ? 'dark' : 'light')) === 'dark'
  r.dataset.theme = dark ? 'light' : 'dark'
  document.querySelectorAll('meta[name=theme-color]').forEach((m) => (m.content = dark ? '#fafafa' : '#000000'))
  try {
    localStorage.setItem('theme', r.dataset.theme)
  } catch {}
})

// Trades: older pages, and a filter to one wallet (click its address). Live trades push
// old rows out past `shown`.
let shown = 50
let maker = ''
let remark = () => {} // the chart redraws the filtered wallet's trades

// Trades and Top traders tabs (traders load when first shown).
function tab(name) {
  const card = tr?.closest('.card')
  if (!card) return
  card.querySelectorAll('[data-tab]').forEach((b) => b.setAttribute('aria-selected', String(b.dataset.tab === name)))
  const traders = $('#traders')
  traders.hidden = name !== 'traders'
  for (const el of [tr.closest('table'), $('#more'), $('#flt')]) if (el) el.hidden = name === 'traders'
  if (name === 'traders' && !traders.dataset.loaded) {
    traders.dataset.loaded = 1
    traders.innerHTML = `<p class="tsub"><i class="sk" style="display:inline-block;width:60%;height:10px;border-radius:5px;background:var(--raise)"></i></p><div class="scroll"><table class="trades tops"><thead><tr><th class="l">#</th><th class="l">Wallet</th><th>Bought</th><th>Sold</th><th>Profit</th><th class="hide-s">Buys / sells</th></tr></thead><tbody>${skeleton(10, SK_TRADERS)}</tbody></table></div>`
    fetch(`/api/traders/${$('#chart').dataset.pool}`)
      .then((r) => r.text())
      .then((h) => (traders.innerHTML = h))
  }
}
document.addEventListener('click', (e) => {
  const b = e.target.closest('[data-tab]')
  if (b) tab(b.dataset.tab)
})
const tr = $('#tr')
async function trades(before) {
  const q = new URLSearchParams()
  if (before) q.set('before', before)
  if (maker) q.set('maker', maker)
  const append = before && !before.startsWith('4294967295')
  if (append) tr.insertAdjacentHTML('beforeend', skeleton(8, SK_TRADES))
  else tr.innerHTML = skeleton(12, SK_TRADES)
  const r = await fetch(`/api/trades/${$('#chart').dataset.pool}?${q}`).then((r) => r.json())
  tr.querySelectorAll('.sk').forEach((s) => s.remove())
  if (append) tr.insertAdjacentHTML('beforeend', r.rows)
  else tr.innerHTML = r.rows
  shown = Math.max(50, tr.rows.length)
  $('#more')?.remove()
  if (r.more) tr.closest('table').insertAdjacentHTML('afterend', r.more)
  remark()
}
document.addEventListener('click', (e) => {
  const more = e.target.closest('#more')
  if (more) return (more.disabled = true), trades(more.dataset.before)
  const mk = e.target.closest('.mk')
  const all = e.target.closest('#flt button')
  if (!mk && !all) return
  maker = mk ? mk.dataset.maker : ''
  $('#flt')?.remove()
  tab('trades')
  if (maker)
    tr.closest('.card').querySelector('.tt').insertAdjacentHTML(
      'afterend',
      `<p class="flt" id="flt">Trades by <span class="mono">${maker.slice(0, 6)}…${maker.slice(-4)}</span> <a class="mono" href="https://basescan.org/address/${maker}" rel="nofollow">Basescan ↗</a><button type="button">Show all</button></p>`
    )
  trades(maker ? '' : '4294967295-4294967295')
})

// Instant search: matching pairs as you type, from the public API (edge-cached per query).
// Token names come from anyone's contract, so results are built as text, never HTML.
const usd = (x, o) => (x == null ? '–' : new Intl.NumberFormat('en', { style: 'currency', currency: 'USD', ...o }).format(x))
document.querySelectorAll('input[type=search]').forEach((input, n) => {
  const box = document.createElement('ul')
  box.className = 'sugg'
  box.id = `sugg${n}`
  box.setAttribute('role', 'listbox')
  box.hidden = true
  input.after(box)
  input.form.classList.add('has-sugg')
  input.setAttribute('role', 'combobox')
  input.setAttribute('aria-autocomplete', 'list')
  input.setAttribute('aria-controls', box.id)
  input.setAttribute('aria-expanded', 'false')
  let timer = 0, active = -1, asked = ''
  const show = (on) => ((box.hidden = !on), input.setAttribute('aria-expanded', String(on)))
  const mark = (i) => {
    const items = box.querySelectorAll('[role=option]')
    active = (i + items.length) % items.length
    items.forEach((el, k) => el.setAttribute('aria-selected', String(k === active)))
    if (items[active]) input.setAttribute('aria-activedescendant', items[active].id)
  }
  const el = (tag, cls, text) => {
    const e = document.createElement(tag)
    if (cls) e.className = cls
    if (text != null) e.textContent = text
    return e
  }
  const avatar = (t) =>
    t.icon ? Object.assign(el('img', 'av'), { src: t.icon, alt: '', width: 24, height: 24 }) : el('span', 'av', (t.symbol[0] || '?').toUpperCase())
  const option = (i, href, parts) => {
    const li = el('li')
    li.id = `${box.id}-${i}`
    li.setAttribute('role', 'option')
    const a = el('a')
    a.href = href
    a.append(...parts)
    li.append(a)
    return li
  }
  // Exact token matches first (their own price, on the token page), then pairs.
  const render = ({ tokens = [], pairs = [] }) => {
    const items = [
      ...tokens.map((t, i) => {
        const name = el('span', 'nm')
        name.append(el('b', '', t.symbol), el('span', 'm', ` ${t.name}`), el('small', '', 'Token'))
        return option(i, t.url, [avatar(t), name, el('span', 'pr', usd(t.price_usd, { maximumSignificantDigits: 4 })), el('span', 'm lq2', '')])
      }),
      ...pairs.slice(0, 8 - tokens.length).map((p, i) => {
        const name = el('span', 'nm')
        name.append(el('b', '', p.base.symbol), el('span', 'm', `/${p.quote.symbol}`), el('small', '', p.dex))
        return option(tokens.length + i, `/base/${p.pair}`, [
          avatar(p.base),
          name,
          el('span', 'pr', usd(p.price_usd, { maximumSignificantDigits: 4 })),
          el('span', 'm lq2', usd(p.liquidity_usd, { notation: 'compact', maximumFractionDigits: 1 })),
        ])
      }),
    ]
    box.replaceChildren(...items)
    active = -1
    show(items.length > 0)
  }
  input.addEventListener('input', () => {
    clearTimeout(timer)
    const q = input.value.trim()
    if (q.length < 2) return show(false)
    if (box.hidden) {
      box.innerHTML = '<li class="sk"><a><i class="c"></i><i style="width:45%"></i></a></li>'.repeat(3)
      show(true)
    }
    timer = setTimeout(async () => {
      asked = q
      const r = await fetch(`/api/v1/search?q=${encodeURIComponent(q)}`).then((r) => (r.ok ? r.json() : { pairs: [] }), () => ({ pairs: [] }))
      if (asked === q && input.value.trim() === q) render(r)
    }, 150)
  })
  input.addEventListener('keydown', (e) => {
    if (box.hidden) return
    if (e.key === 'ArrowDown' || e.key === 'ArrowUp') e.preventDefault(), mark(active + (e.key === 'ArrowDown' ? 1 : -1))
    else if (e.key === 'Escape') show(false)
    else if (e.key === 'Enter' && active >= 0) e.preventDefault(), box.querySelectorAll('a')[active].click()
  })
  input.addEventListener('blur', () => setTimeout(() => show(false), 150))
  input.addEventListener('focus', () => box.children.length && input.value.trim().length > 1 && show(true))
})

// Watchlist: starred pair keys, kept in this browser only.
const watched = () => {
  try {
    return JSON.parse(localStorage.getItem('watch')) || []
  } catch {
    return []
  }
}
const stars = () => {
  const w = new Set(watched())
  document.querySelectorAll('[data-star]').forEach((b) => b.setAttribute('aria-pressed', String(w.has(b.dataset.star))))
}
document.addEventListener('click', (e) => {
  const b = e.target.closest('[data-star]')
  if (!b) return
  e.preventDefault()
  const k = b.dataset.star
  const w = watched()
  try {
    localStorage.setItem('watch', JSON.stringify(w.includes(k) ? w.filter((x) => x !== k) : [k, ...w].slice(0, 100)))
  } catch {}
  stars()
})
stars()

// Pair lists (home tabs, search, watchlist) take fresh rows every few seconds while
// visible. The rows are cached at the edge for 2 s, so every viewer shares one render.
const list = $('[data-list]')
if (list) {
  const watch = list.dataset.list.includes('tab=watch')
  const src = () => (watch ? (watched().length ? `/api/list?pairs=${watched().join(',')}` : '') : list.dataset.list)
  let last = ''
  const refresh = async () => {
    const u = src()
    if (!u) return
    // Not while someone is pointing at or tabbing through the rows: a swap would drop
    // their hover, focus or a click in progress.
    if (list.matches(':hover') || list.contains(document.activeElement)) return
    const rows = await fetch(u).then((r) => (r.ok ? r.text() : ''), () => '')
    if (rows && rows !== last) (list.querySelector('tbody').innerHTML = last = rows), stars()
  }
  if (watch && watched().length) (list.querySelector('tbody').innerHTML = skeleton(Math.min(watched().length, 12), SK_LIST)), refresh()
  setInterval(() => document.hidden || refresh(), 3000)
}

const el = $('#chart')
if (el) chart(el)

async function chart(el) {
  const pool = el.dataset.pool
  let res = +el.dataset.res
  let cs = JSON.parse($('#cd').textContent)
  const w = (await WebAssembly.instantiateStreaming(fetch(el.dataset.wasm))).instance.exports
  const tz = -new Date().getTimezoneOffset() * 60
  let start = NaN, count = 0, cur = null, drag = null, frame = 0, busy = false, done = cs.length < 300
  // Chart options, remembered per browser. Ones the page doesn't offer stay off.
  const opt = { mcap: 0, native: 0, line: 0, log: 0, ma: 0, vwap: 0, rsi: 0 }
  try {
    Object.assign(opt, JSON.parse(localStorage.getItem('chart')))
  } catch {}
  for (const o in opt) if (!$(`[data-o="${o}"]`)) opt[o] = 0
  const supply = +el.dataset.supply
  const latest = el.parentElement.querySelector('.latest')
  // chart::LINE, MA, COMPACT, LOG, RSI, VWAP
  const flags = () => opt.line | (opt.ma << 1) | (opt.mcap << 2) | (opt.log << 3) | (opt.rsi << 4) | (opt.vwap << 5)
  // The filtered wallet's buys and sells, at their price in the chart's unit.
  const marks = () => {
    const k = opt.mcap ? supply : 1
    const ms = maker
      ? [...document.querySelectorAll('#tr tr[data-s="b"], #tr tr[data-s="s"]')].map((r) => [+r.dataset.t, (opt.native ? +r.dataset.n : +r.dataset.u) * k, r.dataset.s === 'b' ? 1 : 0])
      : []
    const p = w.marks(ms.length) // may grow wasm memory, so take the buffer after
    new Float64Array(w.memory.buffer, p, ms.length * 3).set(ms.flat())
  }
  remark = () => (marks(), redraw())

  // Market cap is the price times the supply; volume stays in USD.
  const load = () => {
    const k = opt.mcap ? supply : 1
    const p = w.data(cs.length)
    new Float64Array(w.memory.buffer, p, cs.length * 6).set(cs.flatMap((c) => [c[0], c[1] * k, c[2] * k, c[3] * k, c[4] * k, c[5]]))
    marks()
  }
  const draw = () => {
    frame = 0
    const r = el.getBoundingClientRect()
    const p = w.draw(r.width, r.height, res, start, count, cur ? cur[0] : -1, cur ? cur[1] : -1, tz, flags())
    el.innerHTML = new TextDecoder().decode(new Uint8Array(w.memory.buffer, p, w.len()))
    start = w.start()
    count = w.count()
    if (start < 10 && !done && !busy) older()
    // "Latest" shows once the newest candle has scrolled out of view.
    if (latest) latest.hidden = start + count >= cs.length + 1
  }
  const redraw = () => frame || (frame = requestAnimationFrame(draw))
  const get = (q) => fetch(`/api/candles/${pool}?res=${res}&unit=${opt.native ? 'n' : 'u'}${q}`).then((r) => r.json())
  // Latest candles again (new size or unit), back at the default view.
  async function reload() {
    cs = await get('')
    done = cs.length < 300
    start = NaN
    load()
    draw()
    sync()
  }

  // Candles that closed while the page sat in a cache: take the latest from the API,
  // then the live candle `c` (the API copy may be a few seconds older).
  async function fill(c) {
    const fresh = await get('')
    if (fresh.length) cs = cs.filter((x) => x[0] < fresh[0][0]).concat(fresh)
    const last = cs[cs.length - 1]
    if (last && last[0] === c[0]) cs[cs.length - 1] = c
    else if (!last || c[0] > last[0]) cs.push(c)
    load()
    redraw()
  }

  async function older() {
    busy = true
    const more = await get(`&before=${cs[0][0]}`)
    done = more.length < 300
    if (more.length) {
      cs = more.concat(cs)
      start += more.length
      load()
      redraw()
    }
    busy = false
  }

  load()
  draw()
  new ResizeObserver(redraw).observe(el)

  const pos = (e) => {
    const r = el.getBoundingClientRect()
    return [e.clientX - r.left, e.clientY - r.top]
  }
  // Zoom around `x` (px from the left) to `next` visible candles.
  const zoom = (x, next) => {
    next = Math.min(Math.max(next, 10), Math.max(cs.length + 20, 60))
    const slots = x / (w.plot_w(el.clientWidth) / count)
    start += slots - (slots * next) / count
    count = next
    redraw()
  }
  // Pointers down: one drags (pans), two pinch (touch zoom).
  const down = new Map()
  let pinch = null
  const spread = () => {
    const [a, b] = [...down.values()]
    return [Math.hypot(a[0] - b[0], a[1] - b[1]), (a[0] + b[0]) / 2]
  }
  el.addEventListener('pointermove', (e) => {
    cur = pos(e)
    if (down.has(e.pointerId)) down.set(e.pointerId, cur)
    if (pinch && down.size === 2) {
      const [d, mid] = spread()
      if (d > 0) zoom(mid, (pinch.count * pinch.d) / d)
    } else if (drag) start = drag[1] - (cur[0] - drag[0]) / (w.plot_w(el.clientWidth) / count)
    redraw()
  })
  el.addEventListener('pointerdown', (e) => {
    down.set(e.pointerId, pos(e))
    el.setPointerCapture(e.pointerId)
    if (down.size === 2) (pinch = { d: spread()[0], count }), (drag = null)
    else drag = [pos(e)[0], start]
  })
  const up = (e) => {
    down.delete(e.pointerId)
    pinch = null
    drag = null
  }
  el.addEventListener('pointerup', up)
  el.addEventListener('pointercancel', up)
  el.addEventListener('pointerleave', (e) => {
    up(e)
    cur = null
    redraw()
  })
  // Wheels and trackpads zoom by how far they moved (a mouse notch is ~100 px, a
  // trackpad sends many small steps); pinches arrive as ctrl+wheel; sideways scrolls pan.
  el.addEventListener(
    'wheel',
    (e) => {
      e.preventDefault()
      const px = e.deltaMode === 1 ? 16 : e.deltaMode === 2 ? el.clientHeight : 1
      if (!e.ctrlKey && Math.abs(e.deltaX) > Math.abs(e.deltaY)) {
        start += (e.deltaX * px) / (w.plot_w(el.clientWidth) / count)
        return redraw()
      }
      zoom(pos(e)[0], count * Math.exp(e.deltaY * px * (e.ctrlKey ? 0.01 : 0.0015)))
    },
    { passive: false }
  )

  document.querySelectorAll('.res a').forEach((a) =>
    a.addEventListener('click', async (e) => {
      e.preventDefault()
      res = +a.dataset.res
      document.querySelectorAll('.res a').forEach((x) => x.removeAttribute('aria-current'))
      a.setAttribute('aria-current', 'true')
      history.replaceState(null, '', a.href)
      reload()
    })
  )

  // Toolbar: segmented choices (data-v) and the indicators menu's checkboxes. Indicators
  // that can't draw here are disabled with the reason: VWAP restarts daily, so daily
  // candles have none; averages and RSI need enough candles.
  const card = el.closest('.chartcard')
  const ind = card.querySelector('.ind')
  const need = { ma: 21, rsi: 15, vwap: 1 }
  const sync = () => {
    card.querySelectorAll('button[data-o][data-v]').forEach((b) => b.setAttribute('aria-pressed', String(opt[b.dataset.o] === +b.dataset.v)))
    let on = 0
    card.querySelectorAll('input[data-o]').forEach((c) => {
      const o = c.dataset.o
      const why = o === 'vwap' && res >= 86400 ? 'VWAP restarts each day: pick candles under 1d' : cs.length < (need[o] || 0) ? 'Not enough candles yet' : ''
      c.disabled = !!why
      c.closest('label').title = why
      c.checked = !!opt[o]
      if (opt[o] && !why && o !== 'log') on++
    })
    const n = ind?.querySelector('.cnt')
    if (n) (n.hidden = !on), (n.textContent = on)
    ind?.classList.toggle('on', on > 0)
  }
  const set = (o, v) => {
    if (opt[o] === v) return
    opt[o] = v
    try {
      localStorage.setItem('chart', JSON.stringify(opt))
    } catch {}
    sync()
    if (o === 'native') return reload()
    load()
    draw()
  }
  card.querySelector('.tools')?.addEventListener('click', (e) => {
    const b = e.target.closest('button[data-o]')
    if (!b) return
    if (b.dataset.o === 'full') return document.fullscreenElement ? document.exitFullscreen() : card.requestFullscreen()
    set(b.dataset.o, +b.dataset.v)
  })
  card.addEventListener('change', (e) => {
    const c = e.target.closest('input[data-o]')
    if (c) set(c.dataset.o, c.checked ? 1 : 0)
  })
  // Fullscreen where the browser allows it (not in some frames, nor iPhone Safari).
  const full = card.querySelector('.full')
  if (full) {
    full.hidden = !document.fullscreenEnabled
    document.addEventListener('fullscreenchange', () => {
      const on = document.fullscreenElement === card
      full.innerHTML = `<svg class="i" aria-hidden="true"><use href="#i-${on ? 'minimize' : 'maximize'}"/></svg>`
      full.setAttribute('aria-label', on ? 'Exit fullscreen' : 'Fullscreen')
    })
  }
  const back = () => ((start = NaN), draw())
  latest?.addEventListener('click', back)
  el.addEventListener('dblclick', back)
  sync()
  if (opt.native) reload()
  else if (flags()) load(), draw()

  const rows = $('#tr')
  const live = new EventSource(`/api/live/${pool}?after=${rows ? rows.dataset.last : '0-0'}`)
  live.onmessage = (m) => {
    const u = JSON.parse(m.data)
    if (u.T) document.title = u.T
    for (const [id, [text, cls]] of Object.entries(u.f))
      document.querySelectorAll(`[data-f="${id}"]`).forEach((n) => {
        n.textContent = text
        n.className = cls
        if (cls === 'bar') n.style.setProperty('--r', text) // bar widths travel as text
      })
    const c = u[opt.native ? 'n' : 'c'][res]
    if (c) {
      const last = cs[cs.length - 1]
      if (last && c[0] > last[0] + res) fill(c)
      else if (last && last[0] === c[0]) cs[cs.length - 1] = c
      else if (!last || c[0] > last[0]) cs.push(c)
      load()
      redraw()
    }
    if (rows && u.t) {
      const t = document.createElement('tbody')
      t.innerHTML = u.t
      rows.prepend(...[...t.rows].filter((r) => !maker || r.querySelector('.mk')?.dataset.maker === maker))
      if (maker) marks()
      while (rows.rows.length > shown) rows.deleteRow(-1)
      const m = $('#more')
      if (m) m.dataset.before = rows.rows[rows.rows.length - 1].dataset.k
    }
  }
}
