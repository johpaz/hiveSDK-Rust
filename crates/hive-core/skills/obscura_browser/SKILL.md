---
name: obscura_browser
description: "Browse, read, scrape and fill forms with Obscura, the SDK's only browser: a lightweight headless browser with one live session and element refs"
version: 1.0.0
author: Hive SDK
category: web
permissions:
  - browser_control
dependencies: []
tools: [obscura_navigate, obscura_back, obscura_forward, obscura_reload, obscura_snapshot, obscura_markdown, obscura_links, obscura_interactive_elements, obscura_detect_forms, obscura_extract, obscura_get_attribute, obscura_count, obscura_search, obscura_click, obscura_fill, obscura_type, obscura_fill_form, obscura_press_key, obscura_select_option, obscura_scroll, obscura_wait_for, obscura_wait_for_text, obscura_network_requests, obscura_console_messages, obscura_screenshot, obscura_pdf, obscura_tab_new, obscura_tab_list, obscura_tab_switch, obscura_tab_close, obscura_close]
triggers:
  - "scrapea"
  - "extrae datos de la página"
  - "lee esta página"
  - "llena el formulario"
  - "scrape this page"
  - "read this page"
  - "fill the form"
  - "obscura"
steps:
  - step: 1
    action: obscura_navigate
    instruction: "Open the URL. It returns only the final URL and title and keeps ONE live session for the following calls"
    params:
      url: "target URL"
    output: session
  - step: 2
    action: obscura_snapshot
    instruction: "Read the page as text with element refs (e3, e7…). Use obscura_markdown for article-like content and obscura_links to crawl"
    output: page_text
  - step: 3
    action: act
    instruction: "Act with refs: obscura_click, obscura_fill / obscura_fill_form (one call for a whole form, with optional submit), obscura_press_key, obscura_select_option. Fall back to CSS selectors when no ref exists"
    output: action_result
  - step: 4
    action: verify
    instruction: "Re-read with obscura_snapshot or obscura_wait_for_text to confirm the final state before reporting"
    output: final_state
rules:
  - "The browser is Obscura and nothing else. Never launch, install, suggest or fall back to Chromium, Chrome, Playwright or Puppeteer, and never write scripts that drive them"
  - "Refs are only valid for the last snapshot: take a new obscura_snapshot after navigating or after the DOM changes"
  - "There is one live session: do not interleave calls for two different sites; use obscura_tab_new for a second page"
  - "Obscura blocks localhost and private networks by default (SSRF protection): that is expected, not a bug"
  - "Prefer text: obscura_snapshot, obscura_markdown and obscura_extract. obscura_screenshot and obscura_pdf work only when the installed Obscura supports them; if a tool reports it is unsupported, continue with text instead of looking for another browser"
  - "If Obscura is not installed, tell the user to install it (see docs/obscura.md) and stop; do not substitute another browser"
  - "A timeout or lost connection resets the session: re-navigate and re-snapshot, and never repeat a click, send or purchase whose outcome is unknown"
  - "Never confirm purchases, sends or deletions unless the principal explicitly authorized that effect"
  - "obscura_pdf is only a picture of the page inside a one-page PDF: no text layer, not paginated, not searchable, and office_read_pdf will report it has no extractable text. Use it only to keep a visual copy. To turn a page into a real PDF report, read it with obscura_markdown or obscura_extract and write the PDF with office_write_pdf (see pdf_report_writer)"
output_format:
  structure: markdown
  sections:
    - "url"
    - "findings"
    - "evidence"
  max_length: "Concise"
examples:
  - user_input: "lee los títulos de esta página y sus enlaces"
    expected_behavior: "obscura_navigate → obscura_snapshot → obscura_links → summarize"
  - user_input: "llena el formulario de contacto"
    expected_behavior: "obscura_navigate → obscura_detect_forms → obscura_fill_form with fields → obscura_snapshot to verify"
  - user_input: "hazme un screenshot con Chrome"
    expected_behavior: "Explain that the SDK browser is Obscura, try obscura_screenshot, and if unsupported offer the text snapshot; never start Chrome"
---
# Obscura browser

Obscura is a small headless browser with its own JavaScript engine. It is the only browser this SDK drives, which keeps it light: there is no Chromium download, no Playwright and no Puppeteer.

Typical flow: `obscura_navigate` → `obscura_snapshot` → act with refs → verify. Use `obscura_extract` with a `{field: selector}` map when you need structured data, and `obscura_search`/`obscura_count` as cheap probes before reading a large page.
