# Cascade Theme — derived from wickrunner.com/jh (jobhunter)

Reference CSS: `theme/jh-reference.css` (verbatim extraction, font-faces trimmed).

## Palette (Rosé Pine Dawn variant)

| Token      | Hex       | Use                                   |
|------------|-----------|---------------------------------------|
| `--base`   | `#faf4ed` | window background                     |
| `--surface`| `#fffaf3` | cards, panels, chrome                 |
| `--overlay`| `#f2e9e4` | raised surfaces, popovers             |
| `--hl-low` | `#f4ede8` | hover fills, subtle separators        |
| `--hl-med` | `#dfdad9` | default borders (1.5–2px solid)       |
| `--hl-high`| `#2a2740` | emphasis borders (== ink)             |
| `--ink`    | `#2a2740` | headings, high-contrast text          |
| `--text`   | `#575279` | body text                             |
| `--subtle` | `#797593` | secondary text                        |
| `--muted`  | `#9893a5` | metadata, placeholders                |
| `--love`   | `#b4637a` | primary accent (active states, CTA)   |
| `--rose`   | `#d7827e` | secondary accent                      |
| `--gold`   | `#ea9d34` | warnings / gold CTA                   |
| `--pine`   | `#286983` | info / primary action buttons         |
| `--foam`   | `#56949f` | success, links                        |
| `--iris`   | `#907aa9` | tertiary accent (company names, tags) |

Gradient accent: `linear-gradient(120deg, love 10%, iris 90%)` (text-clip for wordmarks).
Ambient background glows: radial love @8% and iris @10% from top corners.
Optional dotted-paper texture: `radial-gradient(hl-med 1.4px, transparent 1.4px)`, 18px grid.

## Typography

- Display / headers: **Archivo Black** (fallback Fira Sans). Uppercase, letter-spacing 1–2px. Sizes: 44px hero, 30px page title, 19px panel title, 17px empty-state.
- Body: **Fira Sans** 400/600. Body 15–16px, line-height 1.6; meta 13px; labels 12px uppercase 0.5px spacing weight 600.
- Code/transcript: monospace (JetBrains Mono fallback stack) — site has none; pick for app.
- Wordmark: "cascade" in Archivo Black, one word with accent span in `--love`.

## Shape language

- `border-radius: 0` everywhere (except loading spinner). **Hard corners.**
- Borders: 1.5–2px solid `--hl-med`; hover → `--hl-high`; selected → 2px `--love` or `--pine`.
- Cards: surface bg, 2px border, no shadow at rest; hover `translateY(-1px)` + darker border.
- CTA buttons: solid accent bg (pine/love/gold), base-colored text, matching border; hover `brightness(1.08)` + soft colored glow shadow.
- Nav pills: transparent bg, hl-med border, uppercase 11px, active = inverted (ink bg, base text).
- Detail panel entrance: `slideIn 0.25s ease-out` — mirror this with the GTK sidebar Revealer slide.
- Tags/chips: 1px border, 13px, flat.

## App layout mapping

- `.app` max-width 1200px centered, 24px padding → main window content clamp.
- Sidebar (session list) = `.job-list` cards: title (600, 16px), iris subtitle, muted meta row, selected state = 2px love border.
- Transcript = `.detail-panel`; user bubbles surface+pine accent border-left; assistant plain; tool calls = chips/cards with muted mono content.
- Header wordmark "cascade" + `.` in love. Nav: Sessions / New / Plan / Settings as pills.
