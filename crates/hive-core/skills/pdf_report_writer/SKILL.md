---
name: pdf_report_writer
description: "Generate styled PDFs (headings, runs, lists, tables, header/footer with page numbers, themes) with office_write_pdf and verify them by reading them back with office_read_pdf"
version: 1.0.0
author: Hive SDK
category: office
permissions:
  - filesystem_read
  - filesystem_write
dependencies: []
tools: [office_write_pdf, office_read_pdf, obscura_navigate, obscura_markdown, obscura_extract]
triggers:
  - "crear pdf"
  - "generar pdf"
  - "informe en pdf"
  - "exportar a pdf"
  - "pdf con estilos"
  - "create a pdf"
  - "styled pdf"
  - "pdf report"
steps:
  - step: 1
    action: compose
    instruction: "Plan the document as blocks: heading, paragraph (with runs for bold/italic/color/link), list, table, quote, code, rule, page_break. Choose a theme preset (modern, classic, corporate, minimal) and a header/footer"
    output: blocks
  - step: 2
    action: office_write_pdf
    instruction: "Write the PDF with `blocks`, `theme`, `title`, and `footer` such as {\"center\":\"Page {page} of {pages}\"}. Use `content` only for plain text"
    params:
      path: "report.pdf"
    output: file
  - step: 3
    action: office_read_pdf
    instruction: "Read the file back: check the page count, the page text, the footer numbers, that table headers repeat, and that replaced_characters is 0 or acceptable"
    output: verification
rules:
  - "Documents are styled: do not deliver a wall of plain text when the content has structure. Use headings, lists and tables, a theme, and a header/footer with page numbers for anything longer than one page"
  - "Pass either `content` or `blocks`, never both. `content` is plain text only"
  - "Fonts are the standard PDF faces (sans, serif, mono) with Western European text. Characters outside it become ? and are counted in `replaced_characters`: report that to the user and rewrite those characters (for example ✓ as OK) instead of ignoring it"
  - "Links must be absolute http(s) URLs; other schemes are rejected"
  - "Table cells wrap by themselves; give `columns` weights and `align` per column (right for numbers). The header row repeats on every page"
  - "Always verify by reading the PDF back with office_read_pdf. Its text extraction uses real glyph widths, so a wrong result means the document, not the reader; scanned PDFs are reported in warnings, not guessed, and there is no OCR"
  - "When the source is a web page, read it with Obscura (obscura_markdown or obscura_extract) and compose the blocks yourself; never start Chromium, Chrome, Playwright or Puppeteer to print a page. Do not use obscura_pdf for a report: it is only an image of the page, with no text, no pagination and no styling"
  - "Never overwrite an existing file the user did not ask to replace"
output_format:
  structure: markdown
  sections:
    - "file"
    - "pages"
    - "verification"
  max_length: "Concise"
examples:
  - user_input: "hazme un informe en PDF con una tabla de ventas"
    expected_behavior: "compose blocks (heading, paragraph, table with header and zebra), office_write_pdf with theme and footer, office_read_pdf to verify"
  - user_input: "convierte esta página en un PDF"
    expected_behavior: "obscura_navigate → obscura_markdown → compose blocks → office_write_pdf → office_read_pdf; never launch another browser"
---
# Styled PDF reports

`office_write_pdf` lays the document out itself: wrapping uses the real metrics of the PDF fonts, so lines break where a viewer will draw them. Example:

```json
{
  "path": "report.pdf",
  "title": "Quarterly report",
  "theme": {"preset": "corporate"},
  "header": {"left": "{title}", "right": "Confidential", "rule": true},
  "footer": {"center": "Page {page} of {pages}"},
  "blocks": [
    {"type": "heading", "level": 1, "text": "Summary"},
    {"type": "paragraph", "runs": [{"text": "Sales grew "}, {"text": "12%", "bold": true, "color": "B91C1C"}]},
    {"type": "table", "zebra": true, "align": ["left", "right"], "rows": [["Item", "Amount"], ["Licences", "1200.00"]]}
  ]
}
```

Read it back with `office_read_pdf` and confirm the pages, the footer numbering and the table before saying it is done.
