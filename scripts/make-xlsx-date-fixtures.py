#!/usr/bin/env python3
"""Writes the workbooks tests/office.rs reads to check Excel dates, as raw OOXML (no library).

They are small and handmade on purpose: each cell is there to pin one rule of how a number
format decides whether a number is a date. Run from the repository root.
"""
import sys
import zipfile
from pathlib import Path

OUT = Path(sys.argv[1] if len(sys.argv) > 1 else "crates/hive-core/tests/fixtures/office")
HEAD = '<?xml version="1.0" encoding="UTF-8" standalone="yes"?>\n'
NS = 'xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"'

# cellXfs: index -> numFmtId. Custom ids start at 164.
XFS = [0, 14, 22, 164, 21, 165, 9, 166, 167, 17, 20]
FORMATS = {164: "dd/mm/yyyy", 165: "[h]:mm:ss", 166: '"día" 0', 167: "[Red][<0]0.00;0.00"}
S = {name: i for i, name in enumerate(["general", "builtin14", "builtin22", "custom_dmy", "builtin21", "elapsed", "percent", "quoted", "colored", "builtin17", "builtin20"])}


def styles() -> str:
    formats = "".join(f'<numFmt numFmtId="{i}" formatCode="{c.replace(chr(34), "&quot;")}"/>' for i, c in FORMATS.items())
    xfs = "".join(f'<xf numFmtId="{n}" fontId="0" fillId="0" borderId="0" xfId="0"/>' for n in XFS)
    return (f'{HEAD}<styleSheet {NS}><numFmts count="{len(FORMATS)}">{formats}</numFmts><fonts count="1"><font><sz val="11"/></font></fonts>'
            f'<fills count="2"><fill><patternFill patternType="none"/></fill><fill><patternFill patternType="gray125"/></fill></fills>'
            f'<borders count="1"><border><left/><right/><top/><bottom/><diagonal/></border></borders>'
            f'<cellStyleXfs count="1"><xf numFmtId="0" fontId="0" fillId="0" borderId="0"/></cellStyleXfs><cellXfs count="{len(XFS)}">{xfs}</cellXfs></styleSheet>')


def workbook(date1904: bool) -> bytes:
    properties = '<workbookPr date1904="1"/>' if date1904 else ""
    return (f'{HEAD}<workbook {NS} xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">{properties}'
            f'<sheets><sheet name="Fechas" sheetId="1" r:id="rId1"/></sheets></workbook>').encode()


def sheet(rows) -> str:
    out = []
    for n, (label, value, style, kind) in enumerate(rows, start=2):
        t = f' t="{kind}"' if kind else ""
        out.append(f'<row r="{n}"><c r="A{n}" t="inlineStr"><is><t>{label}</t></is></c><c r="B{n}" s="{S[style]}"{t}><v>{value}</v></c></row>')
    head = '<row r="1"><c r="A1" t="inlineStr"><is><t>Etiqueta</t></is></c><c r="B1" t="inlineStr"><is><t>Valor</t></is></c></row>'
    return f'{HEAD}<worksheet {NS}><sheetData>{head}{"".join(out)}</sheetData></worksheet>'


def write(name: str, rows, date1904=False) -> None:
    OUT.mkdir(parents=True, exist_ok=True)
    with zipfile.ZipFile(OUT / name, "w", zipfile.ZIP_DEFLATED) as z:
        z.writestr("[Content_Types].xml", f'{HEAD}<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/><Override PartName="/xl/worksheets/sheet1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/><Override PartName="/xl/styles.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.styles+xml"/></Types>')
        z.writestr("_rels/.rels", f'{HEAD}<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/></Relationships>')
        z.writestr("xl/workbook.xml", workbook(date1904))
        z.writestr("xl/_rels/workbook.xml.rels", f'{HEAD}<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/><Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles" Target="styles.xml"/></Relationships>')
        z.writestr("xl/styles.xml", styles())
        z.writestr("xl/worksheets/sheet1.xml", sheet(rows))


write("dates-1900.xlsx", [
    ("builtin 14", "45717", "builtin14", None),
    ("builtin 22 con hora", "45717.604166666664", "builtin22", None),
    ("personalizado dd/mm/yyyy", "45717", "custom_dmy", None),
    ("hora builtin 21", "0.5", "builtin21", None),
    ("transcurrido [h]:mm:ss", "1.5", "elapsed", None),
    ("1900-02-29 inexistente", "60", "builtin14", None),
    ("primer día", "61", "builtin14", None),
    ("serie 1", "1", "builtin14", None),
    ("porcentaje", "0.25", "percent", None),
    ("general", "45717", "general", None),
    ("texto entre comillas", "5", "quoted", None),
    ("color y condición", "-2.5", "colored", None),
    ("mmm-yy", "45717", "builtin17", None),
    ("celda ISO t=d", "2025-03-01T00:00:00", "builtin14", "d"),
    ("fuera de rango", "3000000", "builtin14", None),
    ("mm:ss builtin 20", "0.25", "builtin20", None),
])
write("dates-1904.xlsx", [("sistema 1904", "43891", "builtin14", None), ("hora", "0.75", "builtin21", None)], date1904=True)
