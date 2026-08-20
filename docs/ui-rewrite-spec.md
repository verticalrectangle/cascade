# cascade-gtk UI Rewrite Spec (v0.2)

Replace the cascade-gtk UI wholesale with a Rust port of the omperator linux-port GTK app.
Backend contract (worker.rs Cmd/UiMsg, cascade-core) is UNCHANGED. This is a UI-only rewrite.

## Reference sources (READ THESE FIRST)
- `/home/alexis/dev/omperator/apps/linux/Sources/T4CodeLinux/AppWindow.swift` — layout, rail, composer, streaming, scroll pinning (1815 lines)
- `.../TranscriptWidgets.swift` — entry widgets: user bubble, prose, code blocks, tool cards, advisory cards, images, copy chrome
- `.../MarkdownRenderer.swift`, `.../SyntaxHighlighter.swift`
- `.../PanesFactory.swift` — browser pane (WebKitGTK)
- `.../themes/theme-dawn.css` — PORT VERBATIM (GTK4 CSS is compatible). Default theme.
- `.../themes/theme-moon.css` — port verbatim; toggle via ◐ in topbar.
- Screenshot reference: `/home/alexis/dev/omperator/apps/site/public/screenshots/linux-dawn.png`
- jh palette notes: `/home/alexis/dev/cascade/docs/theme.md`

## Non-negotiables (user directives)
- FULL WIDTH layout. Rail is a fixed-width left column (default 232px); transcript fills ALL remaining space. NO centered narrow column, NO max-width clamp on the window content.
- Sleek controls: glyph buttons (☰ ◐ ⤢-less ⚙ ▤ ➤ 📎), no giant text pills. Only text buttons: rail "New session" is the `+` flat-btn; settings toggles are checkboxes.
- No "TERMINAL" text badge — status is a colored glyph/label like omperator's ACTIVE/IDLE.
- jh/omperator palette: cream base #FAF4ED, surface cards, ink #2A2740, love #B4637A, pine #286983, gold #EA9D34, iris #907AA9, foam #56949F. Borders hairline #DFDAD9 (1px), hard corners.
- Dense chrome, airy transcript. Assistant prose serif (New York/Charter/Georgia, 17-18px), chrome sans 11pt.

## Feature checklist (port ALL)
### Window/topbar
- [ ] Slim topbar: ☰ rail toggle | status label (session title / "connecting…") | model pill | Inbox badge (count) | ▤ panes | ⚙ settings | ◐ theme
- [ ] Settings popover (checkboxes: Dark mode, Show sidebar)
- [ ] Single-instance behavior (gtk4 Application default; present existing window on activate)
- [ ] Keyboard: Ctrl+B rail toggle, Ctrl+N new session, Esc closes lightbox

### Rail (left)
- [ ] GtkRevealer slide open/close (220ms), wrapper with overflow hidden + halign start (see Swift comments — avoids double-relayout)
- [ ] Drag-resize divider 180–400px, persist width in settings; drag left of min snaps closed; drag from collapsed edge reopens
- [ ] Header: "Sessions" + `+` (new session in current project) + ◐ theme
- [ ] Search entry, live-filters title/project/status/model
- [ ] Segmented grouping: Recent (Running/Saved sections) / Project — collapsible sections
- [ ] Rows: title, model, context-usage progress bar (from RpcSessionState.contextUsage), status label (ACTIVE gold = streaming, IDLE, CLOSED), relative timestamp refreshed ~30s
- [ ] Row click → open session; selected row styling `.rail-item-selected`

### Transcript
- [ ] Durable rows box + live-tail box (streaming widgets live below durable rows)
- [ ] User: right-aligned bubble card (.user-bubble). Assistant: serif prose with markdown (h1-h3, bold gold, italic, inline code, quote, lists, links) + syntax-highlighted code blocks (header = lang + copy button)
- [ ] Tool cards: collapsible (chevron ▸/▾), header row "NAME · intent/phase", body = mono result text; kind-tinted left border (tool-use pine, result foam, thinking iris)
- [ ] Thinking: collapsible card, muted italic
- [ ] Advisory/notice cards with severity variants (info pine, error love)
- [ ] Bottom-pin scroll: follow stream only when within 48px of bottom; user scroll up releases; programmatic scroll must not release; per-session scroll memory; frame-tick follow (add_tick_callback after layout)
- [ ] Copy: right-click context menu (Copy text / Copy code) + hover ghost copy button on code blocks
- [ ] Plan state: render as a plan card in transcript? NO — plan panel stays a right slide-over: auto-opens when TodoChanged arrives with non-empty phases, auto-closes when empty. Phase headers + task glyphs (○ ◐ ● ✕ strikethrough).

### Composer
- [ ] Full-width bordered box; 📎 attach (file dialog for images), attachment chips strip w/ remove ×, Ctrl+V image paste, drag&drop image files
- [ ] Auto-grow multiline to ~6 lines then internal scroll; Enter send / Shift+Enter newline; placeholder "Message…"
- [ ] Send ➤ small glyph button; while streaming: Stop + Queue buttons (queue = follow_up via steer); hint line "esc stops a running turn"
- [ ] Esc → abort (turn cancel)

### Questions (ask tool / approvals) — cascade-specific, keep our flow
- [ ] Question card floating above composer: 1.5px ink border, surface bg, question text 15px semibold, options as compact full-width rows (love fill on hover), "Other" free-text row; confirm = allow(pine)/deny(love) small buttons; input/editor variants; open_url shows URL + Open button; timeout countdown if present

### Inbox (ported, cascade semantics)
- [ ] Inbox badge in topbar = count of unseen events (turn completed in non-selected session, question pending anywhere, errors). Click → dropdown list, click item → jump to session, clears entry.

### Browser pane (right sidebar)
- [ ] ▤ toggles right sidebar with WebKitGTK WebView (crate `webkit6`), per-session URL memory (persist in settings), drag-resize 280–600px

### Onboarding/login
- [ ] First-run overlay card on dimmed workspace: "Welcome to Cascade", email+password, sign in → CloudClient::login; auto-hides if token valid; error line; ALSO a "Use locally without account" link → local-only mode.

## Backend mapping (worker.rs — keep, adapt minimally)
- Cmd: Login, Logout, SaveCloudUrl, RefreshSessions, NewSession{kind,cwd,model}, OpenSession{id,kind,join_handle}, Prompt, Abort, AutotestOpen — KEEP ALL. Add: Queue(message) → steer; InboxOpen; PaneToggle(url).
- UiMsg: keep; add InboxCount(usize).
- SessionEvent → widgets: TextDelta → live text (committed/tail split per Swift applySplitLabels); ThinkingDelta → thinking card; ToolStart/Update/End → tool cards; TodoChanged → plan panel; UiRequest → question card; AgentEnd → settle live tail into durable rows, bump inbox if not focused session.

## Constraints
- Rust + gtk4-rs only (add `webkit6 = "2"` for browser; images via gtk4 gdk::Texture).
- Keep CASCADE_AUTOTEST hook working (main.rs) — steps unchanged.
- cargo check -p cascade-gtk must pass; do not touch other crates.
- No Electron patterns, no centered narrow column, no giant text pills.
