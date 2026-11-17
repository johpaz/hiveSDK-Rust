#!/usr/bin/env node
// Writes sample Office files with this SDK so a person can open them in Microsoft Office (or Google
// Docs, Keynote, Numbers…) and check they look as described in each file's own text.
//   node scripts/office-samples.mjs ./samples
import { mkdirSync } from 'node:fs';
import { resolve } from 'node:path';
import { NativeTools } from '../packages/npm/index.mjs';

const out = resolve(process.argv[2] ?? 'office-samples');
mkdirSync(out, { recursive: true });
const tools = new NativeTools({ root: out, office: {} });
const long = Array.from({ length: 90 }, (_, n) => ({ text: `Párrafo ${n + 1}: texto de relleno para que el documento ocupe varias páginas y se vea el encabezado y el pie en cada una.` }));

await tools.execute('office_write_docx', { path: 'informe.docx', title: 'Informe de prueba', paragraphs: [
  { text: 'Qué debe verse', type: 'title1' },
  { text: 'Arriba, en cada página, a la derecha: «Confidencial — Informe de prueba». Abajo, centrado: «Página N de M» con los números correctos (se actualizan solos).' },
  ...long],
  header: { text: 'Confidencial — {title}', align: 'right' }, footer: { text: 'Página {page} de {pages}', align: 'center' } });

await tools.execute('office_write_xlsx', { path: 'ventas.xlsx', sheets: [{ name: 'Ventas', headers: ['fecha', 'hora', 'monto', 'tasa', 'nota'],
  data: [
    { fecha: '2025-03-01', hora: '14:30', monto: 1234.5, tasa: 0.25, nota: 'Debe verse 01/03/2025 o 2025-03-01 como FECHA, 1.234,50 en moneda, 25 % y 14:30' },
    { fecha: '2025-12-31', hora: '09:05:30', monto: 99, tasa: 0.075, nota: '=1+1 es texto, no una fórmula' }],
  columns: [{ column: 'fecha', format: 'date', width: 14 }, { column: 'hora', format: 'time' }, { column: 'monto', format: 'currency', bold: true, color: '#CC0000' },
            { column: 'tasa', format: 'percent', fill: '#FFFF99' }, { column: 'nota', wrap: true, width: 50 }],
  header_style: { bold: true, fill: '#DDEEFF', align: 'center' }, freeze_header: true, autofilter: true }] });

await tools.execute('office_write_pptx', { path: 'presentacion.pptx', presentation_title: 'Presentación de prueba', presentation_subtitle: 'Notas del orador',
  slides: [{ title: 'Diapositiva uno', points: ['Primer punto', 'Segundo punto'], notes: 'Estas son las notas del orador de la diapositiva uno.\nSegunda línea de notas.' },
           { title: 'Diapositiva dos', content: 'Sin notas.' },
           { title: 'Diapositiva tres', content: 'Con notas.', notes: 'Notas de la tres.' }] });

await tools.execute('office_write_pdf', { path: 'documento.pdf', blocks: [{ type: 'heading', level: 1, text: 'PDF de prueba' }, { type: 'paragraph', text: 'Debe verse con estilo, no como texto plano.' }],
  header: { center: '{title}' }, footer: { center: 'Página {page} de {pages}' }, title: 'PDF de prueba' }).catch(error => console.log('pdf: ', String(error.message ?? error).slice(0, 120)));

await tools.close();
console.log(`Archivos en ${out}: informe.docx, ventas.xlsx, presentacion.pptx, documento.pdf`);
