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

// Older trades, a page at a time. Live trades push old rows out past `shown`.
let shown = 50
$('#more')?.addEventListener('click', async function more(e) {
  const b = e.currentTarget
  b.disabled = true
  const r = await fetch(`/api/trades/${$('#chart').dataset.pool}?before=${b.dataset.before}`).then((r) => r.json())
  $('#tr').insertAdjacentHTML('beforeend', r.rows)
  shown = $('#tr').rows.length
  if (r.more) (b.outerHTML = r.more), $('#more').addEventListener('click', more)
  else b.remove()
})

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
  el.addEventListener('pointermove', (e) => {
    cur = pos(e)
    if (drag) {
      start = drag[1] - (cur[0] - drag[0]) / (w.plot_w(el.clientWidth) / count)
    }
    redraw()
  })
  el.addEventListener('pointerdown', (e) => {
    drag = [pos(e)[0], start]
    el.setPointerCapture(e.pointerId)
  })
  el.addEventListener('pointerup', () => (drag = null))
  el.addEventListener('pointerleave', () => {
    cur = drag = null
    redraw()
  })
  el.addEventListener(
    'wheel',
    (e) => {
      e.preventDefault()
      const x = pos(e)[0] / (w.plot_w(el.clientWidth) / count) // slots from the left edge
      const next = Math.min(Math.max(count * (e.deltaY > 0 ? 1.12 : 0.89), 10), Math.max(cs.length + 20, 60))
      start += x - (x * next) / count
      count = next
      redraw()
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
      rows.insertAdjacentHTML('afterbegin', u.t)
      while (rows.rows.length > shown) rows.deleteRow(-1)
      const m = $('#more')
      if (m) m.dataset.before = rows.rows[rows.rows.length - 1].dataset.k
    }
  }
}
