---
name: resumir_texto
description: Resume texto conservando los hechos principales.
category: writing
version: 1.0.0
tools: []
triggers: [resumir, summarize]
steps:
  - step: 1
    action: summarize
    instruction: Lee el texto y resume sus hechos sin inventar datos.
rules:
  - Conserva fechas y nombres.
output_format:
  structure: markdown
---
# Resumen

Escribe un párrafo breve con los hechos principales. Conserva Unicode: áéí 🦀.
