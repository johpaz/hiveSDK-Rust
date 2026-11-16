import assert from 'node:assert/strict';
import { test } from 'node:test';
import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { NativeTools } from '../index.mjs';

test('office tools write and read Word, Excel and PowerPoint inside the sandbox', async () => {
  const root = mkdtempSync(path.join(tmpdir(), 'hive-office-'));
  try {
    assert.ok(!(await new NativeTools({ root }).definitions()).some(tool => tool.name.startsWith('office_')), 'disabled by default');
    const tools = new NativeTools({ root, office: { max_rows_per_sheet: 100 } });
    const names = (await tools.definitions()).map(tool => tool.name).filter(name => name.startsWith('office_'));
    assert.deepEqual(names, ['office_read_docx', 'office_write_docx', 'office_read_xlsx', 'office_write_xlsx', 'office_read_pdf', 'office_write_pdf', 'office_read_pptx', 'office_write_pptx']);

    await tools.execute('office_write_docx', { path: 'docs/a.docx', create_dirs: true, title: 'Informe',
      paragraphs: [{ text: 'Capítulo', type: 'title1' }, { text: 'hola 🦀' }], tables: [{ header: true, rows: [{ cells: ['x', 'y'] }, { cells: ['1', '2'] }] }] });
    const doc = await tools.execute('office_read_docx', { path: 'docs/a.docx' });
    assert.deepEqual(doc.paragraphs.map(p => [p.text, p.style]), [['Informe', 'Title'], ['Capítulo', 'Heading1'], ['hola 🦀', null]]);
    assert.deepEqual(doc.tables, [[['x', 'y'], ['1', '2']]]);

    await tools.execute('office_write_xlsx', { path: 'b.xlsx', sheets: [{ name: 'Datos', headers: ['k', 'n'], data: [{ k: 'a', n: 1 }, { k: '=1+1', n: 2.5 }] }] });
    const book = await tools.execute('office_read_xlsx', { path: 'b.xlsx' });
    assert.deepEqual(book.data.Datos, [{ k: 'a', n: 1 }, { k: '=1+1', n: 2.5 }]);

    await tools.execute('office_write_pptx', { path: 'c.pptx', presentation_title: 'Portada', presentation_subtitle: 'Sub', theme: { preset: 'dark', colors: { accent: 'ff00aa' } }, slides: [{ title: 'Uno', points: ['a', 'b'] }] });
    const deck = await tools.execute('office_read_pptx', { path: 'c.pptx', only_slide: 2 });
    assert.deepEqual(deck.slides.map(s => s.paragraphs), [['Uno', 'a', 'b']]);
    assert.deepEqual((await tools.execute('office_read_pptx', { path: 'c.pptx', only_slide: 1 })).slides[0].paragraphs, ['Portada', 'Sub']);
    await assert.rejects(tools.execute('office_write_pptx', { path: 'bad.pptx', theme: { preset: 'neon' }, slides: [{ title: 'x' }] }), /do not match schema/);

    await tools.execute('office_write_pdf', { path: 'd.pdf', title: 'Informe', content: 'Hola 🦀 canción\nsegunda línea', page_size: 'Letter' });
    const pdf = await tools.execute('office_read_pdf', { path: 'd.pdf' });
    assert.deepEqual([pdf.total_pages, pdf.text, pdf.metadata.title], [1, 'Informe\nHola ? canción\nsegunda línea', 'Informe']);

    await assert.rejects(tools.execute('office_read_docx', { path: '../outside.docx' }), /relative/);
    await assert.rejects(tools.execute('office_read_xlsx', { path: 'a.docx' }), /\.xlsx/);
    const agentTools = await tools.createTools();
    assert.equal(typeof agentTools.find(tool => tool.name === 'office_read_docx').execute, 'function');
    assert.throws(() => new NativeTools({ root, office: { max_sheets: 0 } }), /out of range/);
  } finally { rmSync(root, { recursive: true, force: true }); }
});
