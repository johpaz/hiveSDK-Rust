---
name: office_document_manager
description: "Read, create and verify Word, Excel, PowerPoint and PDF files inside the workspace sandbox"
version: 1.0.0
author: Hive SDK
category: office
permissions:
  - filesystem_read
  - filesystem_write
dependencies: []
tools: [office_read_pdf, office_write_pdf, office_read_docx, office_write_docx, office_read_xlsx, office_write_xlsx, office_read_pptx, office_write_pptx]
triggers:
  - "leer pdf"
  - "crear pdf"
  - "leer word"
  - "crear word"
  - "leer excel"
  - "crear excel"
  - "crear presentación"
  - "read a document"
  - "create a spreadsheet"
steps:
  - step: 1
    action: inspect
    instruction: "Pick the tool by extension. Paths are relative to the sandbox root and must end with the matching extension"
    output: tool_choice
  - step: 2
    action: office_read_xlsx
    instruction: "Read before you change: for data use office_read_xlsx (sheet, range), for documents office_read_docx, for decks office_read_pptx, for PDFs office_read_pdf"
    output: content
  - step: 3
    action: verify
    instruction: "After writing, read the file back with the matching read tool and check the result"
    output: verification
rules:
  - "Only .docx, .xlsx, .pptx and .pdf are supported. Legacy .doc/.xls/.ppt, macros and embedded objects are not handled; say so instead of converting by guesswork"
  - "Formulas are never executed: office_read_xlsx returns cached values"
  - "Presentations are styled: choose a theme preset (modern, light, dark, corporate, warm) and keep slides short, using `points` for bullets"
  - "PDFs are styled too: use `blocks`, a theme and a header/footer with page numbers (see pdf_report_writer); `content` is for plain text only"
  - "PDF reading uses real glyph widths; encrypted PDFs open with the empty password or the user `password` you pass to office_read_pdf (a wrong one is an error), fonts without a Unicode mapping and scanned images are reported in warnings, not guessed, and there is no OCR"
  - "Never use Chromium, Chrome, Playwright or Puppeteer to convert documents. The SDK browser is Obscura and the Office tools need no browser"
  - "Do not overwrite a file the user did not ask to replace"
output_format:
  structure: markdown
  sections:
    - "file"
    - "summary"
  max_length: "Concise"
examples:
  - user_input: "lee el Excel de ventas y resume por mes"
    expected_behavior: "office_read_xlsx → summarize"
  - user_input: "crea una presentación corporativa de 5 láminas"
    expected_behavior: "office_write_pptx with theme preset corporate → office_read_pptx to verify"
---
# Office documents

Word, Excel, PowerPoint and PDF are read and written by the SDK itself, inside the filesystem sandbox. Every file read is size-limited and processing has a time limit; failures are reported with the reason.
