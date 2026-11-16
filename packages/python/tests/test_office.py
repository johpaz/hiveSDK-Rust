import tempfile
import unittest
from hive_sdk import NativeTools


class OfficeToolTests(unittest.IsolatedAsyncioTestCase):
    async def test_write_and_read_word_excel_and_powerpoint_inside_the_sandbox(self):
        with tempfile.TemporaryDirectory() as root:
            self.assertFalse(any(t['name'].startswith('office_') for t in await NativeTools({'root': root}).definitions()))
            tools = NativeTools({'root': root, 'office': {'max_rows_per_sheet': 100}})
            names = [t['name'] for t in await tools.definitions() if t['name'].startswith('office_')]
            self.assertEqual(names, ['office_read_docx', 'office_write_docx', 'office_read_xlsx', 'office_write_xlsx', 'office_read_pdf', 'office_write_pdf', 'office_read_pptx', 'office_write_pptx'])

            await tools.execute('office_write_docx', {'path': 'docs/a.docx', 'create_dirs': True, 'title': 'Informe',
                'paragraphs': [{'text': 'Capítulo', 'type': 'title1'}, {'text': 'hola 🦀'}],
                'tables': [{'header': True, 'rows': [{'cells': ['x', 'y']}, {'cells': ['1', '2']}]}]})
            doc = await tools.execute('office_read_docx', {'path': 'docs/a.docx'})
            self.assertEqual([(p['text'], p['style']) for p in doc['paragraphs']], [('Informe', 'Title'), ('Capítulo', 'Heading1'), ('hola 🦀', None)])
            self.assertEqual(doc['tables'], [[['x', 'y'], ['1', '2']]])

            await tools.execute('office_write_xlsx', {'path': 'b.xlsx', 'sheets': [{'name': 'Datos', 'headers': ['k', 'n'], 'data': [{'k': 'a', 'n': 1}, {'k': '=1+1', 'n': 2.5}]}]})
            book = await tools.execute('office_read_xlsx', {'path': 'b.xlsx'})
            self.assertEqual(book['data']['Datos'], [{'k': 'a', 'n': 1}, {'k': '=1+1', 'n': 2.5}])

            await tools.execute('office_write_pptx', {'path': 'c.pptx', 'presentation_title': 'Portada', 'presentation_subtitle': 'Sub',
                'theme': {'preset': 'dark', 'colors': {'accent': 'ff00aa'}}, 'slides': [{'title': 'Uno', 'points': ['a', 'b']}]})
            deck = await tools.execute('office_read_pptx', {'path': 'c.pptx', 'only_slide': 2})
            self.assertEqual([s['paragraphs'] for s in deck['slides']], [['Uno', 'a', 'b']])
            cover = await tools.execute('office_read_pptx', {'path': 'c.pptx', 'only_slide': 1})
            self.assertEqual(cover['slides'][0]['paragraphs'], ['Portada', 'Sub'])
            with self.assertRaisesRegex(Exception, 'do not match schema'):
                await tools.execute('office_write_pptx', {'path': 'bad.pptx', 'theme': {'preset': 'neon'}, 'slides': [{'title': 'x'}]})

            await tools.execute('office_write_pdf', {'path': 'd.pdf', 'title': 'Informe', 'content': 'Hola 🦀 canción\nsegunda línea', 'page_size': 'Letter'})
            pdf = await tools.execute('office_read_pdf', {'path': 'd.pdf'})
            self.assertEqual((pdf['total_pages'], pdf['text'], pdf['metadata']['title']), (1, 'Informe\nHola ? canción\nsegunda línea', 'Informe'))

            with self.assertRaisesRegex(Exception, 'relative'):
                await tools.execute('office_read_docx', {'path': '../outside.docx'})
            with self.assertRaisesRegex(Exception, r'\.xlsx'):
                await tools.execute('office_read_xlsx', {'path': 'a.docx'})
            with self.assertRaisesRegex(Exception, 'out of range'):
                NativeTools({'root': root, 'office': {'max_sheets': 0}})


if __name__ == '__main__':
    unittest.main()
