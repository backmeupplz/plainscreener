// plainscreener: the chart is server-rendered; this adds pan/zoom/crosshair (via the
// same Rust renderer compiled to wasm), live updates, and copy buttons.
const $ = (s, el = document) => el.querySelector(s)

document.addEventListener('click', (e) => {
  const b = e.target.closest('[data-copy]')
  if (!b) return
  navigator.clipboard.writeText(b.dataset.copy).then(() => {
    const t = b.textContent
    b.textContent = 'Copied'
    setTimeout(() => (b.textContent = t), 1200)
  })
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
const tr = $('#tr')
async function trades(before) {
  const q = new URLSearchParams()
  if (before) q.set('before', before)
  if (maker) q.set('maker', maker)
  const r = await fetch(`/api/trades/${$('#chart').dataset.pool}?${q}`).then((r) => r.json())
  if (before && !before.startsWith('4294967295')) tr.insertAdjacentHTML('beforeend', r.rows)
  else tr.innerHTML = r.rows
  shown = Math.max(50, tr.rows.length)
  $('#more')?.remove()
  if (r.more) tr.closest('table').insertAdjacentHTML('afterend', r.more)
}
document.addEventListener('click', (e) => {
  const more = e.target.closest('#more')
  if (more) return (more.disabled = true), trades(more.dataset.before)
  const mk = e.target.closest('.mk')
  const all = e.target.closest('#flt button')
  if (!mk && !all) return
  maker = mk ? mk.dataset.maker : ''
  $('#flt')?.remove()
  if (maker)
    tr.closest('.card').querySelector('h2').insertAdjacentHTML(
      'afterend',
      `<p class="flt" id="flt">Trades by <span class="mono">${maker.slice(0, 6)}…${maker.slice(-4)}</span> <a class="mono" href="https://basescan.org/address/${maker}" rel="nofollow">Basescan ↗</a><button type="button">Show all</button></p>`
    )
  trades(maker ? '' : '4294967295-4294967295')
})

// Pair lists (home tabs, search) take fresh rows every few seconds while visible. The rows
// are cached at the edge for 2 s, so every viewer shares one render.
const list = $('[data-list]')
if (list) {
  let last = ''
  setInterval(async () => {
    if (document.hidden) return
    const rows = await fetch(list.dataset.list).then((r) => (r.ok ? r.text() : ''), () => '')
    if (rows && rows !== last) list.querySelector('tbody').innerHTML = last = rows
  }, 3000)
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
  const opt = { mcap: 0, native: 0, line: 0, ma: 0 }
  try {
    Object.assign(opt, JSON.parse(localStorage.getItem('chart')))
  } catch {}
  for (const o in opt) if (!$(`[data-o="${o}"]`)) opt[o] = 0
  const supply = +el.dataset.supply
  const flags = () => opt.line | (opt.ma << 1) | (opt.mcap << 2) // chart::LINE, MA, COMPACT

  // Market cap is the price times the supply; volume stays in USD.
  const load = () => {
    const k = opt.mcap ? supply : 1
    const p = w.data(cs.length)
    new Float64Array(w.memory.buffer, p, cs.length * 6).set(cs.flatMap((c) => [c[0], c[1] * k, c[2] * k, c[3] * k, c[4] * k, c[5]]))
  }
  const draw = () => {
    frame = 0
    const r = el.getBoundingClientRect()
    const p = w.draw(r.width, r.height, res, start, count, cur ? cur[0] : -1, cur ? cur[1] : -1, tz, flags())
    el.innerHTML = new TextDecoder().decode(new Uint8Array(w.memory.buffer, p, w.len()))
    start = w.start()
    count = w.count()
    if (start < 10 && !done && !busy) older()
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

  const card = el.closest('.chartcard')
  const press = () =>
    card.querySelectorAll('[data-o]').forEach((b) => {
      const o = b.dataset.o
      if (o in opt) b.setAttribute('aria-pressed', b.dataset.v ? String(opt[o] === +b.dataset.v) : String(!!opt[o]))
    })
  card.querySelector('.tools')?.addEventListener('click', (e) => {
    const b = e.target.closest('[data-o]')
    if (!b) return
    const o = b.dataset.o
    if (o === 'reset') return (start = NaN), draw()
    if (o === 'full') return document.fullscreenElement ? document.exitFullscreen() : card.requestFullscreen()
    const prev = opt[o]
    opt[o] = b.dataset.v ? +b.dataset.v : 1 - opt[o]
    if (opt[o] === prev) return
    try {
      localStorage.setItem('chart', JSON.stringify(opt))
    } catch {}
    press()
    if (o === 'native') return reload()
    load()
    draw()
  })
  el.addEventListener('dblclick', () => ((start = NaN), draw()))
  press()
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
      while (rows.rows.length > shown) rows.deleteRow(-1)
      const m = $('#more')
      if (m) m.dataset.before = rows.rows[rows.rows.length - 1].dataset.k
    }
  }
}
