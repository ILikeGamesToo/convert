// Regenerates the office-document fixtures in this directory.
//
//   node make-office-fixtures.mjs [output-directory]
//
// Every byte is written here: a DOCX, XLSX or PPTX is a ZIP of XML parts, an
// ODT or ODS is the same with an OpenDocument `mimetype` entry, and the XLS is
// a BIFF8 workbook inside an OLE2 compound file. Nothing is copied from a real
// document, so there is no third-party licence in these files. The output is
// deterministic: one fixed timestamp per ZIP entry, no compression-level or
// container randomisation.
//
// This script is test tooling, not part of the app.

import { mkdirSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';
import { crc32, deflateRawSync } from 'node:zlib';

const out = process.argv[2] ?? '.';
mkdirSync(out, { recursive: true });
const write = (name, bytes) => writeFileSync(join(out, name), bytes);

// ---- ZIP ------------------------------------------------------------------

const utf8 = (text) => Buffer.from(text, 'utf8');

function zip(parts) {
  const local = [];
  const central = [];
  let offset = 0;
  for (const part of parts) {
    const name = utf8(part.name);
    const raw = Buffer.isBuffer(part.data) ? part.data : utf8(part.data);
    const method = part.store ? 0 : 8;
    const body = method === 0 ? raw : deflateRawSync(raw);
    const crc = crc32(raw) >>> 0;

    const header = Buffer.alloc(30 + name.length);
    header.writeUInt32LE(0x04034b50, 0);
    header.writeUInt16LE(20, 4);
    header.writeUInt16LE(0, 6);
    header.writeUInt16LE(method, 8);
    header.writeUInt16LE(0, 10);
    header.writeUInt16LE(0x0021, 12);
    header.writeUInt32LE(crc, 14);
    header.writeUInt32LE(body.length, 18);
    header.writeUInt32LE(raw.length, 22);
    header.writeUInt16LE(name.length, 26);
    header.writeUInt16LE(0, 28);
    name.copy(header, 30);
    local.push(header, body);

    const entry = Buffer.alloc(46 + name.length);
    entry.writeUInt32LE(0x02014b50, 0);
    entry.writeUInt16LE(20, 4);
    entry.writeUInt16LE(20, 6);
    entry.writeUInt16LE(0, 8);
    entry.writeUInt16LE(method, 10);
    entry.writeUInt16LE(0, 12);
    entry.writeUInt16LE(0x0021, 14);
    entry.writeUInt32LE(crc, 16);
    entry.writeUInt32LE(body.length, 20);
    entry.writeUInt32LE(raw.length, 24);
    entry.writeUInt16LE(name.length, 28);
    entry.writeUInt16LE(0, 30);
    entry.writeUInt16LE(0, 32);
    entry.writeUInt16LE(0, 34);
    entry.writeUInt16LE(0, 36);
    entry.writeUInt32LE(0, 38);
    entry.writeUInt32LE(offset, 42);
    name.copy(entry, 46);
    central.push(entry);

    offset += header.length + body.length;
  }
  const directory = Buffer.concat(central);
  const end = Buffer.alloc(22);
  end.writeUInt32LE(0x06054b50, 0);
  end.writeUInt16LE(0, 4);
  end.writeUInt16LE(0, 6);
  end.writeUInt16LE(parts.length, 8);
  end.writeUInt16LE(parts.length, 10);
  end.writeUInt32LE(directory.length, 12);
  end.writeUInt32LE(offset, 16);
  end.writeUInt16LE(0, 20);
  return Buffer.concat([...local, directory, end]);
}

const contentTypes = `<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
<Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
<Default Extension="xml" ContentType="application/xml"/>
<Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/>
<Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/>
<Override PartName="/ppt/presentation.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.presentation.main+xml"/>
</Types>`;

const rootRels = `<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/>
</Relationships>`;

const ooxml = (parts) => zip([{ name: '[Content_Types].xml', data: contentTypes }, ...parts]);

// ---- DOCX -----------------------------------------------------------------

const wordml = (body) => `<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main" xmlns:wp="http://schemas.openxmlformats.org/drawingml/2006/wordprocessingDrawing" xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main">
<w:body>${body}<w:sectPr><w:pgSz w:w="11906" w:h="16838"/></w:sectPr></w:body>
</w:document>`;

const textbox = `<w:r><w:drawing><wp:inline><a:graphic><a:graphicData><w:txbxContent><w:p><w:r><w:t>TEXT BOX TEXT</w:t></w:r></w:p></w:txbxContent></a:graphicData></a:graphic></wp:inline></w:drawing></w:r>`;

const docxBody = [
  '<w:p><w:pPr><w:pStyle w:val="Heading1"/><w:outlineLvl w:val="0"/></w:pPr><w:r><w:t>Quarterly report</w:t></w:r></w:p>',
  '<w:p>',
  '<w:r><w:t xml:space="preserve">The first paragraph, with a tab</w:t></w:r>',
  '<w:r><w:tab/></w:r>',
  '<w:r><w:t xml:space="preserve">and a line break</w:t></w:r>',
  '<w:r><w:br/></w:r>',
  '<w:r><w:t>that continues here.</w:t></w:r>',
  '<w:del><w:r><w:delText>deleted text stays out</w:delText></w:r></w:del>',
  '<w:r><w:fldChar w:fldCharType="begin"/></w:r>',
  '<w:r><w:instrText>PAGE</w:instrText></w:r>',
  textbox,
  '</w:p>',
  '<w:p/>',
  '<w:p><w:pPr><w:pStyle w:val="Heading 2"/></w:pPr><w:r><w:t>Details</w:t></w:r></w:p>',
  '<w:p><w:pPr><w:numPr><w:ilvl w:val="0"/><w:numId w:val="1"/></w:numPr></w:pPr><w:r><w:t>first bullet</w:t></w:r></w:p>',
  '<w:p><w:pPr><w:numPr><w:ilvl w:val="1"/><w:numId w:val="1"/></w:numPr></w:pPr><w:r><w:t>nested bullet</w:t></w:r></w:p>',
  '<w:tbl>',
  '<w:tblPr/><w:tblGrid/>',
  '<w:tr><w:tc><w:p><w:r><w:t>Item</w:t></w:r></w:p></w:tc><w:tc><w:p><w:r><w:t>Quantity</w:t></w:r></w:p></w:tc><w:tc><w:p><w:r><w:t>Notes</w:t></w:r></w:p></w:tc></w:tr>',
  '<w:tr><w:tc><w:p><w:r><w:t>Widgets</w:t></w:r></w:p></w:tc><w:tc><w:p><w:r><w:t>3</w:t></w:r></w:p></w:tc><w:tc><w:p><w:r><w:t>new</w:t></w:r></w:p></w:tc></w:tr>',
  '<w:tr><w:tc><w:p><w:r><w:t>Gadgets</w:t></w:r></w:p></w:tc><w:tc><w:p><w:r><w:t>4</w:t></w:r></w:p></w:tc><w:tc><w:p/></w:tc></w:tr>',
  '<w:tr><w:tc><w:tcPr><w:gridSpan w:val="2"/></w:tcPr><w:p><w:r><w:t>Spanned</w:t></w:r></w:p></w:tc><w:tc><w:p><w:r><w:t>last</w:t></w:r></w:p></w:tc></w:tr>',
  '</w:tbl>',
  '<w:p><w:r><w:t xml:space="preserve">A closing paragraph with an &amp; in it.</w:t></w:r></w:p>',
].join('');

write(
  'sample-office.docx',
  ooxml([
    { name: '_rels/.rels', data: rootRels.replace('word/document.xml', 'word/document.xml') },
    { name: 'word/document.xml', data: wordml(docxBody) },
  ]),
);

write(
  'sample-office-empty.docx',
  ooxml([{ name: 'word/document.xml', data: wordml('') }]),
);

write(
  'sample-office-corrupt.docx',
  zip([{ name: '[Content_Types].xml', data: contentTypes }]),
);

write(
  'sample-office-plain.zip',
  zip([
    { name: 'notes.txt', data: 'not an office document\n' },
    { name: 'readme.txt', data: 'just a zip of text files\n' },
  ]),
);

const encryptedPackage = Buffer.concat([
  Buffer.from([0xd0, 0xcf, 0x11, 0xe0, 0xa1, 0xb1, 0x1a, 0xe1]),
  Buffer.alloc(472),
  Buffer.from('EncryptedPackage', 'utf16le'),
  Buffer.alloc(512),
]);
write('sample-office-encrypted.docx', encryptedPackage);

// ---- PPTX -----------------------------------------------------------------

const presentationMl = `<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:presentation xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">
<p:sldIdLst><p:sldId id="256" r:id="rId7"/><p:sldId id="257" r:id="rId3"/></p:sldIdLst>
</p:presentation>`;

const presentationRels = `<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
<Relationship Id="rId3" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slide" Target="slides/slide1.xml"/>
<Relationship Id="rId7" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slide" Target="slides/slide2.xml"/>
</Relationships>`;

const slide = (text) => `<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:sld xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main" xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main">
<p:cSld><p:spTree><p:sp><p:txBody><a:bodyPr/><a:p><a:r><a:t>${text}</a:t></a:r></a:p></p:txBody></p:sp></p:spTree></p:cSld>
</p:sld>`;

write(
  'sample-office.pptx',
  ooxml([
    {
      name: '_rels/.rels',
      data: rootRels.replace('Target="word/document.xml"', 'Target="ppt/presentation.xml"'),
    },
    { name: 'ppt/presentation.xml', data: presentationMl },
    { name: 'ppt/_rels/presentation.xml.rels', data: presentationRels },
    {
      name: 'ppt/slides/slide1.xml',
      data: slide('First slide part, second in the deck'),
    },
    {
      name: 'ppt/slides/slide2.xml',
      data: slide('Second slide part, first in the deck'),
    },
  ]),
);

// ---- ODT and ODS ----------------------------------------------------------

const manifest = (encrypted) => `<?xml version="1.0" encoding="UTF-8"?>
<manifest:manifest xmlns:manifest="urn:oasis:names:tc:opendocument:xmlns:manifest:1.0" manifest:version="1.3">
<manifest:file-entry manifest:full-path="/" manifest:media-type="application/vnd.oasis.opendocument.text"/>
<manifest:file-entry manifest:full-path="content.xml" manifest:media-type="text/xml"${encrypted ? '><manifest:encryption-data manifest:checksum-type="SHA1"/></manifest:file-entry' : '/>'}
</manifest:manifest>`;

const odtContent = `<?xml version="1.0" encoding="UTF-8"?>
<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" office:version="1.3">
<office:automatic-styles/>
<office:body><office:text>
<text:h text:outline-level="1">OpenDocument heading</text:h>
<text:p>A paragraph with<text:s text:c="3"/>three spaces and a tab<text:tab/>here.</text:p>
<text:list><text:list-item><text:p>one</text:p></text:list-item><text:list-item><text:p>two</text:p></text:list-item></text:list>
<table:table table:name="Table1">
<table:table-row><table:table-cell><text:p>Name</text:p></table:table-cell><table:table-cell><text:p>Value</text:p></table:table-cell></table:table-row>
<table:table-row><table:table-cell><text:p>Alpha</text:p></table:table-cell><table:table-cell><text:p>1</text:p></table:table-cell></table:table-row>
<table:table-row><table:table-cell><text:p>Beta</text:p></table:table-cell><table:table-cell><text:p>2</text:p></table:table-cell></table:table-row>
<table:table-row><table:table-cell table:number-columns-repeated="2"><text:p>Gamma</text:p></table:table-cell></table:table-row>
<table:table-row table:number-rows-repeated="1048576"><table:table-cell table:number-columns-repeated="16384"/></table:table-row>
</table:table>
</office:text></office:body>
</office:document-content>`;

const odtMimetype = 'application/vnd.oasis.opendocument.text';

write(
  'sample-office.odt',
  zip([
    { name: 'mimetype', data: odtMimetype, store: true },
    { name: 'META-INF/manifest.xml', data: manifest(false) },
    { name: 'content.xml', data: odtContent },
  ]),
);

write(
  'sample-office-encrypted.odt',
  zip([
    { name: 'mimetype', data: odtMimetype, store: true },
    { name: 'META-INF/manifest.xml', data: manifest(true) },
    { name: 'content.xml', data: 'encrypted bytes' },
  ]),
);

const odsMimetype = 'application/vnd.oasis.opendocument.spreadsheet';
const odsContent = `<?xml version="1.0" encoding="UTF-8"?>
<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0" office:version="1.3"><office:body><office:spreadsheet><table:table table:name="Data"><table:table-row><table:table-cell office:value-type="string"><text:p>Name</text:p></table:table-cell><table:table-cell office:value-type="string"><text:p>Count</text:p></table:table-cell></table:table-row><table:table-row><table:table-cell office:value-type="string"><text:p>third</text:p></table:table-cell><table:table-cell office:value-type="float" office:value="3"><text:p>3</text:p></table:table-cell></table:table-row><table:table-row><table:table-cell office:value-type="string"><text:p>fourth</text:p></table:table-cell><table:table-cell office:value-type="float" office:value="4"><text:p>4</text:p></table:table-cell></table:table-row></table:table></office:spreadsheet></office:body></office:document-content>`;

write(
  'sample-office.ods',
  zip([
    { name: 'mimetype', data: odsMimetype, store: true },
    { name: 'META-INF/manifest.xml', data: manifest(false).replace('vnd.oasis.opendocument.text', 'vnd.oasis.opendocument.spreadsheet') },
    { name: 'content.xml', data: odsContent },
  ]),
);

// ---- XLSX -----------------------------------------------------------------

const sharedStrings = ['Item', 'Quantity', 'Price', 'In stock', 'Updated', 'Widgets', 'Gadgets', 'A note in the second sheet'];

const sharedStringsXml = `<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<sst xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" count="${sharedStrings.length}" uniqueCount="${sharedStrings.length}">${sharedStrings
  .map((text) => `<si><t>${text}</t></si>`)
  .join('')}</sst>`;

const stylesXml = `<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<styleSheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
<fonts count="1"><font><sz val="11"/><name val="Calibri"/></font></fonts>
<fills count="1"><fill><patternFill patternType="none"/></fill></fills>
<borders count="1"><border/></borders>
<cellStyleXfs count="1"><xf numFmtId="0" fontId="0" fillId="0" borderId="0"/></cellStyleXfs>
<cellXfs count="2"><xf numFmtId="0" fontId="0" fillId="0" borderId="0" xfId="0"/><xf numFmtId="14" fontId="0" fillId="0" borderId="0" xfId="0" applyNumberFormat="1"/></cellXfs>
</styleSheet>`;

// A shared string cell, a number cell, a boolean cell and a date-styled cell.
const cell = (reference, body, extra = '') => `<c r="${reference}"${extra}>${body}</c>`;
const sharedCell = (reference, index) => cell(reference, `<v>${index}</v>`, ' t="s"');
const numberCell = (reference, value) => cell(reference, `<v>${value}</v>`);
const booleanCell = (reference, value) => cell(reference, `<v>${value ? 1 : 0}</v>`, ' t="b"');
const dateCell = (reference, serial) => cell(reference, `<v>${serial}</v>`, ' s="1"');

const inventorySheet = `<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData>
<row r="1">${sharedCell('A1', 0)}${sharedCell('B1', 1)}${sharedCell('C1', 2)}${sharedCell('D1', 3)}${sharedCell('E1', 4)}</row>
<row r="2">${sharedCell('A2', 5)}${numberCell('B2', 3)}${numberCell('C2', 1.5)}${booleanCell('D2', true)}${dateCell('E2', 45360)}</row>
<row r="3">${sharedCell('A3', 6)}${numberCell('B3', 4)}${numberCell('C3', 2.25)}${booleanCell('D3', false)}${dateCell('E3', 45361.5)}</row>
</sheetData></worksheet>`;

const notesSheet = `<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData>
<row r="1">${sharedCell('A1', 7)}</row>
</sheetData></worksheet>`;

const workbookXml = `<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">
<sheets><sheet name="Inventory" sheetId="1" r:id="rId1"/><sheet name="Notes" sheetId="2" r:id="rId2"/></sheets>
</workbook>`;

const workbookRels = `<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/>
<Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet2.xml"/>
<Relationship Id="rId3" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/sharedStrings" Target="sharedStrings.xml"/>
<Relationship Id="rId4" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles" Target="styles.xml"/>
</Relationships>`;

write(
  'sample-office.xlsx',
  zip([
    { name: '[Content_Types].xml', data: contentTypes },
    {
      name: '_rels/.rels',
      data: rootRels.replace('Target="word/document.xml"', 'Target="xl/workbook.xml"'),
    },
    { name: 'xl/workbook.xml', data: workbookXml },
    { name: 'xl/_rels/workbook.xml.rels', data: workbookRels },
    { name: 'xl/sharedStrings.xml', data: sharedStringsXml },
    { name: 'xl/styles.xml', data: stylesXml },
    { name: 'xl/worksheets/sheet1.xml', data: inventorySheet },
    { name: 'xl/worksheets/sheet2.xml', data: notesSheet },
  ]),
);

// ---- XLS ------------------------------------------------------------------

// BIFF8 records. Each record is a type, a length and its payload.
const record = (type, data) => {
  const header = Buffer.alloc(4);
  header.writeUInt16LE(type, 0);
  header.writeUInt16LE(data.length, 2);
  return Buffer.concat([header, data]);
};

const bof = (substream) => {
  const data = Buffer.alloc(16);
  data.writeUInt16LE(0x0600, 0);
  data.writeUInt16LE(substream, 2);
  data.writeUInt16LE(0x0dbb, 4);
  data.writeUInt16LE(0x07cc, 6);
  return record(0x0809, data);
};
const endOfFile = () => record(0x000a, Buffer.alloc(0));

const dimensions = (rows, columns) => {
  const data = Buffer.alloc(14);
  data.writeUInt32LE(0, 0);
  data.writeUInt32LE(rows - 1, 4);
  data.writeUInt16LE(0, 8);
  data.writeUInt16LE(columns - 1, 10);
  return record(0x0200, data);
};

const biffLabel = (row, column, stringIndex) => {
  const data = Buffer.alloc(10);
  data.writeUInt16LE(row, 0);
  data.writeUInt16LE(column, 2);
  data.writeUInt16LE(0, 4);
  data.writeUInt32LE(stringIndex, 6);
  return record(0x00fd, data);
};

const biffNumber = (row, column, value) => {
  const data = Buffer.alloc(14);
  data.writeUInt16LE(row, 0);
  data.writeUInt16LE(column, 2);
  data.writeUInt16LE(0, 4);
  data.writeDoubleLE(value, 6);
  return record(0x0203, data);
};

const biffBoolean = (row, column, value) => {
  const data = Buffer.alloc(8);
  data.writeUInt16LE(row, 0);
  data.writeUInt16LE(column, 2);
  data.writeUInt16LE(0, 4);
  data[6] = value ? 1 : 0;
  data[7] = 0;
  return record(0x0205, data);
};

const sharedStringTable = (strings) => {
  const head = Buffer.alloc(8);
  head.writeUInt32LE(strings.length, 0);
  head.writeUInt32LE(strings.length, 4);
  const entries = strings.map((text) => {
    const chars = Buffer.from(text, 'latin1');
    const header = Buffer.alloc(3);
    header.writeUInt16LE(chars.length, 0);
    header[2] = 0;
    return Buffer.concat([header, chars]);
  });
  return record(0x00fc, Buffer.concat([head, ...entries]));
};

const boundSheet = (position, name) => {
  const chars = Buffer.from(name, 'latin1');
  const data = Buffer.alloc(8 + chars.length);
  data.writeUInt32LE(position, 0);
  data.writeUInt16LE(0, 4);
  data[6] = chars.length;
  data[7] = 0;
  chars.copy(data, 8);
  return record(0x0085, data);
};

const bi = ['Name', 'Count', 'Flag', 'first', 'second'];
const sheetSubstream = Buffer.concat([
  bof(0x0010),
  dimensions(3, 3),
  biffLabel(0, 0, 0),
  biffLabel(0, 1, 1),
  biffLabel(0, 2, 2),
  biffLabel(1, 0, 3),
  biffNumber(1, 1, 1),
  biffBoolean(1, 2, true),
  biffLabel(2, 0, 4),
  biffNumber(2, 1, 2),
  biffBoolean(2, 2, false),
  endOfFile(),
]);

const globalsLength =
  bof(0x0005).length +
  sharedStringTable(bi).length +
  boundSheet(0, 'Legacy').length +
  endOfFile().length;
const workbookStream = Buffer.concat([
  bof(0x0005),
  sharedStringTable(bi),
  boundSheet(globalsLength, 'Legacy'),
  endOfFile(),
  sheetSubstream,
]);

// An OLE2 compound file holding that stream as the `Workbook` entry. The stream
// is padded past the 4096-byte mini-stream cutoff so it lives in ordinary
// sectors, which keeps the container to a header, one FAT sector, one directory
// sector and the stream itself.
function compoundFile(stream) {
  const sector = 512;
  const body = Buffer.alloc(Math.max(4096, Math.ceil(stream.length / sector) * sector));
  stream.copy(body);
  const bodySectors = body.length / sector;

  const fat = Buffer.alloc(sector, 0xff);
  fat.writeUInt32LE(0xfffffffd, 0);
  fat.writeUInt32LE(0xfffffffe, 4);
  for (let index = 0; index < bodySectors; index += 1) {
    const id = 2 + index;
    fat.writeUInt32LE(index === bodySectors - 1 ? 0xfffffffe : id + 1, id * 4);
  }

  const directory = Buffer.alloc(sector);
  const entry = (index, name, type, start, size) => {
    const at = index * 128;
    const chars = Buffer.from(name, 'utf16le');
    chars.copy(directory, at);
    directory.writeUInt16LE(chars.length + 2, at + 64);
    directory[at + 66] = type;
    directory[at + 67] = 1;
    directory.writeUInt32LE(0xffffffff, at + 68);
    directory.writeUInt32LE(0xffffffff, at + 72);
    directory.writeUInt32LE(0xffffffff, at + 76);
    directory.writeUInt32LE(start, at + 116);
    directory.writeUInt32LE(size, at + 120);
  };
  entry(0, 'Root Entry', 5, 0xfffffffe, 0);
  entry(1, 'Workbook', 2, 2, body.length);

  const header = Buffer.alloc(sector);
  Buffer.from([0xd0, 0xcf, 0x11, 0xe0, 0xa1, 0xb1, 0x1a, 0xe1]).copy(header, 0);
  header.writeUInt16LE(0x003e, 24);
  header.writeUInt16LE(0x0003, 26);
  header.writeUInt16LE(0xfffe, 28);
  header.writeUInt16LE(0x0009, 30);
  header.writeUInt16LE(0x0006, 32);
  header.writeUInt32LE(1, 44);
  header.writeUInt32LE(1, 48);
  header.writeUInt32LE(4096, 56);
  header.writeUInt32LE(0xfffffffe, 60);
  header.writeUInt32LE(0, 64);
  header.writeUInt32LE(0xfffffffe, 68);
  header.writeUInt32LE(0, 72);
  header.fill(0xff, 76, 512);
  header.writeUInt32LE(0, 76);
  return Buffer.concat([header, fat, directory, body]);
}

write('sample-office.xls', compoundFile(workbookStream));

console.log(`wrote office fixtures to ${out}`);
