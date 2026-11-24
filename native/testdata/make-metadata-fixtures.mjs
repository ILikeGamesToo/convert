// Regenerates the metadata fixtures in this directory.
//
//   node make-metadata-fixtures.mjs [output-directory]
//
// The pixels and the tone are written here, the EXIF block, the ICC profile and
// the ID3v2/FLAC tag blocks are built here, and the M4A and Opus containers are
// muxed by FFmpeg from the same generated sources. Nothing is copied from a
// third-party file. The FFmpeg muxes embed the encoder version, so those two
// files will not be byte-identical across FFmpeg releases; the rest are
// deterministic. This script is test tooling, not part of the app.

import { spawnSync } from 'node:child_process';
import { mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';
import { crc32, deflateSync } from 'node:zlib';

const out = process.argv[2] ?? '.';
mkdirSync(out, { recursive: true });
const write = (name, bytes) => writeFileSync(join(out, name), bytes);

// ---- Base pixels and audio ------------------------------------------------

const width = 32;
const height = 24;
const raw = Buffer.alloc((width * 3 + 1) * height);
let at = 0;
for (let y = 0; y < height; y++) {
  raw[at++] = 0;
  for (let x = 0; x < width; x++) {
    raw[at++] = Math.round((x * 255) / (width - 1));
    raw[at++] = Math.round((y * 255) / (height - 1));
    raw[at++] = (x * 7 + y * 11) % 256;
  }
}
const basePng = Buffer.concat([
  Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]),
  pngChunk('IHDR', ihdr(width, height)),
  pngChunk('IDAT', deflateSync(raw)),
  pngChunk('IEND', Buffer.alloc(0)),
]);
write('base.png', basePng);

const rate = 44100;
const frames = rate;
const samples = Buffer.alloc(frames * 2);
for (let i = 0; i < frames; i++) {
  const value = Math.round(Math.sin((i / rate) * 440 * Math.PI * 2) * 0.5 * 32767);
  samples.writeInt16LE(value, i * 2);
}
const wav = Buffer.alloc(44);
wav.write('RIFF', 0, 'ascii');
wav.writeUInt32LE(36 + samples.length, 4);
wav.write('WAVE', 8, 'ascii');
wav.write('fmt ', 12, 'ascii');
wav.writeUInt32LE(16, 16);
wav.writeUInt16LE(1, 20);
wav.writeUInt16LE(1, 22);
wav.writeUInt32LE(rate, 24);
wav.writeUInt32LE(rate * 2, 28);
wav.writeUInt16LE(2, 32);
wav.writeUInt16LE(16, 34);
wav.write('data', 36, 'ascii');
wav.writeUInt32LE(samples.length, 40);
write('base.wav', Buffer.concat([wav, samples]));

// ---- Small helpers --------------------------------------------------------

function u16be(value) {
  const bytes = Buffer.alloc(2);
  bytes.writeUInt16BE(value, 0);
  return bytes;
}
function u32be(value) {
  const bytes = Buffer.alloc(4);
  bytes.writeUInt32BE(value, 0);
  return bytes;
}
function u32le(value) {
  const bytes = Buffer.alloc(4);
  bytes.writeUInt32LE(value, 0);
  return bytes;
}
function ihdr(width, height) {
  const bytes = Buffer.alloc(13);
  bytes.writeUInt32BE(width, 0);
  bytes.writeUInt32BE(height, 4);
  bytes[8] = 8;
  bytes[9] = 2;
  return bytes;
}
function pngChunk(type, data) {
  const head = Buffer.alloc(8);
  head.writeUInt32BE(data.length, 0);
  head.write(type, 4, 'ascii');
  return Buffer.concat([head, data, u32be(crc32(Buffer.concat([head.subarray(4), data])) >>> 0)]);
}

// ---- EXIF TIFF stream -----------------------------------------------------

const ascii = (text) => Buffer.from(text + '\0', 'latin1');
const short = (...values) => {
  const bytes = Buffer.alloc(values.length * 2);
  values.forEach((value, index) => bytes.writeUInt16LE(value, index * 2));
  return bytes;
};
const rational = (...pairs) => {
  const bytes = Buffer.alloc(pairs.length * 8);
  pairs.forEach(([n, d], index) => {
    bytes.writeUInt32LE(n, index * 8);
    bytes.writeUInt32LE(d, index * 8 + 4);
  });
  return bytes;
};

const ifd0 = [
  [0x010f, 2, ascii('Convert2it')],
  [0x0110, 2, ascii('Metadata Test')],
  [0x0112, 3, short(6)],
  [0x011a, 5, rational([72, 1])],
  [0x011b, 5, rational([72, 1])],
  [0x0128, 3, short(2)],
  [0x0131, 2, ascii('convert2it')],
  [0x0132, 2, ascii('2026:11:24 12:00:00')],
];
const exifIfd = [
  [0x9003, 2, ascii('2026:11:24 12:00:00')],
  [0x829a, 5, rational([1, 125])],
  [0x829d, 5, rational([28, 10])],
  [0x8827, 3, short(200)],
  [0x920a, 5, rational([35, 1])],
  [0xa434, 2, ascii('Test Lens')],
];
const gpsIfd = [
  [0x0000, 1, Buffer.from([2, 3, 0, 0])],
  [0x0001, 2, ascii('N')],
  [0x0002, 5, rational([40, 1], [26, 1], [4600, 100])],
];

const typeUnit = [1, 1, 2, 4, 8, 1, 1, 2, 4, 8, 4, 8];
function directorySize(fields, entries) {
  let size = 2 + entries * 12 + 4;
  for (const [, , value] of fields) {
    if (value.length > 4) size += value.length + (value.length & 1);
  }
  return size;
}
function writeDirectory(buffer, fields, base) {
  const dataStart = base + 2 + fields.length * 12 + 4;
  buffer.writeUInt16LE(fields.length, base);
  const data = [];
  let dataAt = dataStart;
  let entry = base + 2;
  for (const [tag, type, value] of fields) {
    const count = value.length / typeUnit[type - 1];
    buffer.writeUInt16LE(tag, entry);
    buffer.writeUInt16LE(type, entry + 2);
    buffer.writeUInt32LE(count, entry + 4);
    if (value.length <= 4) {
      value.copy(buffer, entry + 8);
    } else {
      buffer.writeUInt32LE(dataAt, entry + 8);
      data.push({ at: dataAt, value });
      dataAt += value.length + (value.length & 1);
    }
    entry += 12;
  }
  buffer.writeUInt32LE(0, base + 2 + fields.length * 12);
  for (const { at, value } of data) value.copy(buffer, at);
  return dataAt;
}
function buildExif() {
  const ifd0Size = directorySize(ifd0, ifd0.length + 2);
  const exifOffset = 8 + ifd0Size;
  const exifSize = directorySize(exifIfd, exifIfd.length);
  const gpsOffset = exifOffset + exifSize;
  const gpsSize = directorySize(gpsIfd, gpsIfd.length);
  const buffer = Buffer.alloc(8 + ifd0Size + exifSize + gpsSize);
  buffer.write('II', 0, 'ascii');
  buffer.writeUInt16LE(42, 2);
  buffer.writeUInt32LE(8, 4);
  let at = writeDirectory(
    buffer,
    [
      ...ifd0,
      [0x8769, 4, u32le(exifOffset)],
      [0x8825, 4, u32le(gpsOffset)],
    ],
    8,
  );
  at = writeDirectory(buffer, exifIfd, at);
  writeDirectory(buffer, gpsIfd, at);
  return buffer;
}

// ---- ICC profile: a minimal v2 RGB matrix profile -------------------------

function buildIcc(label) {
  const desc = Buffer.from(label + '\0', 'latin1');
  const descType = Buffer.alloc(12);
  descType.write('desc', 0, 'ascii');
  const descTag = Buffer.concat([
    descType,
    u32be(desc.length),
    desc,
    Buffer.alloc(4 + 4 + 2 + 1 + 67),
  ]);
  const whitePoint = Buffer.alloc(8 + 12);
  whitePoint.write('XYZ ', 0, 'ascii');
  whitePoint.writeUInt32BE(0x0000f6d6, 8);
  whitePoint.writeUInt32BE(0x00010000, 12);
  whitePoint.writeUInt32BE(0x0000d32d, 16);
  const header = Buffer.alloc(128);
  header.writeUInt32BE(128 + 4 + 2 * 12 + descTag.length + whitePoint.length, 0);
  header.writeUInt32BE(0x04300000, 4);
  header.writeUInt32BE(0x02100000, 8);
  header.write('mntr', 12, 'ascii');
  header.write('RGB ', 16, 'ascii');
  header.write('XYZ ', 20, 'ascii');
  header.writeUInt16BE(2026, 24);
  header.writeUInt16BE(11, 26);
  header.writeUInt16BE(24, 28);
  header.writeUInt16BE(12, 30);
  header.writeUInt16BE(0, 32);
  header.writeUInt16BE(0, 34);
  header.write('acsp', 36, 'ascii');
  header.writeUInt32BE(0x0000f6d6, 68);
  header.writeUInt32BE(0x00010000, 72);
  header.writeUInt32BE(0x0000d32d, 76);
  const table = Buffer.alloc(4 + 2 * 12);
  table.writeUInt32BE(2, 0);
  table.write('desc', 4, 'ascii');
  table.writeUInt32BE(128 + table.length, 8);
  table.writeUInt32BE(descTag.length, 12);
  table.write('wtpt', 16, 'ascii');
  table.writeUInt32BE(128 + table.length + descTag.length, 20);
  table.writeUInt32BE(whitePoint.length, 24);
  return Buffer.concat([header, table, descTag, whitePoint]);
}

// ---- JPEG: EXIF in APP1, ICC in APP2 -------------------------------------

function injectJpeg(base, exif, icc) {
  const app1 = Buffer.concat([
    Buffer.from([0xff, 0xe1]),
    u16be(2 + 6 + exif.length),
    Buffer.from('Exif\0\0', 'latin1'),
    exif,
  ]);
  const chunkSize = 65533 - 14;
  const parts = Math.ceil(icc.length / chunkSize);
  const chunks = [];
  for (let index = 0; index < parts; index++) {
    const slice = icc.subarray(index * chunkSize, (index + 1) * chunkSize);
    const body = Buffer.concat([
      Buffer.from('ICC_PROFILE\0', 'latin1'),
      Buffer.from([index + 1, parts]),
      slice,
    ]);
    chunks.push(Buffer.concat([Buffer.from([0xff, 0xe2]), u16be(2 + body.length), body]));
  }
  return Buffer.concat([base.subarray(0, 2), app1, ...chunks, base.subarray(2)]);
}

// ---- PNG: EXIF in eXIf, ICC in iCCP --------------------------------------

function injectPng(base, exif, icc) {
  const iccp = Buffer.concat([
    Buffer.from('convert2it\0', 'latin1'),
    Buffer.from([0]),
    deflateSync(icc),
  ]);
  const parts = [
    base.subarray(0, 8),
    pngChunk('IHDR', base.subarray(16, 16 + 13)),
    pngChunk('iCCP', iccp),
    pngChunk('eXIf', exif),
  ];
  let at = 8;
  while (at < base.length) {
    const length = base.readUInt32BE(at);
    const type = base.subarray(at + 4, at + 8).toString('ascii');
    if (type !== 'IHDR') parts.push(base.subarray(at, at + 12 + length));
    at += 12 + length;
  }
  return Buffer.concat(parts);
}

// ---- MP3: ID3v2.3 with an APIC cover -------------------------------------

function utf16(text) {
  const bytes = Buffer.alloc(2 + text.length * 2);
  bytes[0] = 0xff;
  bytes[1] = 0xfe;
  for (let index = 0; index < text.length; index++) {
    bytes.writeUInt16LE(text.charCodeAt(index), 2 + index * 2);
  }
  return bytes;
}
function id3Frame(id, payload) {
  return Buffer.concat([
    Buffer.from(id, 'ascii'),
    u32be(payload.length),
    Buffer.from([0, 0]),
    payload,
  ]);
}
function synchsafe(length) {
  return Buffer.from([
    (length >> 21) & 0x7f,
    (length >> 14) & 0x7f,
    (length >> 7) & 0x7f,
    length & 0x7f,
  ]);
}
function coverPng() {
  const size = 8;
  const rows = Buffer.alloc((size * 3 + 1) * size);
  let at = 0;
  for (let y = 0; y < size; y++) {
    rows[at++] = 0;
    for (let x = 0; x < size; x++) {
      rows[at++] = x * 32;
      rows[at++] = y * 32;
      rows[at++] = 128;
    }
  }
  return Buffer.concat([
    Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]),
    pngChunk('IHDR', ihdr(size, size)),
    pngChunk('IDAT', deflateSync(rows)),
    pngChunk('IEND', Buffer.alloc(0)),
  ]);
}
function injectMp3(base) {
  const frames = [
    id3Frame('TIT2', Buffer.concat([Buffer.from([1]), utf16('Waves')])),
    id3Frame('TPE1', Buffer.concat([Buffer.from([1]), utf16('Grüße')])),
    id3Frame('TALB', Buffer.concat([Buffer.from([1]), utf16('Metadata Album')])),
    id3Frame('TPE2', Buffer.concat([Buffer.from([1]), utf16('Album Artist')])),
    id3Frame('TRCK', Buffer.concat([Buffer.from([1]), utf16('3/12')])),
    id3Frame('TCON', Buffer.concat([Buffer.from([1]), utf16('Ambient')])),
    id3Frame('TYER', Buffer.concat([Buffer.from([1]), utf16('2026')])),
    id3Frame(
      'COMM',
      Buffer.concat([
        Buffer.from([1]),
        Buffer.from('eng', 'ascii'),
        utf16(''),
        utf16('recorded for the metadata test'),
      ]),
    ),
    id3Frame(
      'APIC',
      Buffer.concat([
        Buffer.from([0]),
        Buffer.from('image/png', 'ascii'),
        Buffer.from([0, 3, 0]),
        coverPng(),
      ]),
    ),
  ];
  const body = Buffer.concat(frames);
  return Buffer.concat([
    Buffer.from('ID3', 'ascii'),
    Buffer.from([3, 0, 0]),
    synchsafe(body.length),
    body,
    base,
  ]);
}

// ---- FLAC: Vorbis comments and a PICTURE block ---------------------------

function injectFlac(base) {
  const blocks = [];
  let at = 4;
  for (let guard = 0; guard < 64; guard++) {
    const header = base[at];
    const length = (base[at + 1] << 16) | (base[at + 2] << 8) | base[at + 3];
    blocks.push({ kind: header & 0x7f, data: base.subarray(at + 4, at + 4 + length) });
    at += 4 + length;
    if (header & 0x80) break;
  }
  const vendor = Buffer.from('convert2it', 'ascii');
  const comments = [
    'TITLE=Waves',
    'ARTIST=Grüße',
    'ALBUM=Metadata Album',
    'ALBUMARTIST=Album Artist',
    'TRACKNUMBER=3',
    'TRACKTOTAL=12',
    'GENRE=Ambient',
    'DATE=2026-11-24',
    'COMMENT=recorded for the metadata test',
  ];
  const commentBlock = Buffer.concat([
    u32le(vendor.length),
    vendor,
    u32le(comments.length),
    ...comments.map((entry) => {
      const bytes = Buffer.from(entry, 'utf8');
      return Buffer.concat([u32le(bytes.length), bytes]);
    }),
  ]);
  const cover = coverPng();
  const mime = Buffer.from('image/png', 'ascii');
  const pictureBlock = Buffer.concat([
    u32be(3),
    u32be(mime.length),
    mime,
    u32be(0),
    u32be(0),
    u32be(0),
    u32be(0),
    u32be(0),
    u32be(cover.length),
    cover,
  ]);
  blocks.splice(1, 0, { kind: 4, data: commentBlock }, { kind: 6, data: pictureBlock });
  const parts = [Buffer.from('fLaC', 'ascii')];
  blocks.forEach((block, index) => {
    const header = Buffer.alloc(4);
    header[0] = (index === blocks.length - 1 ? 0x80 : 0) | block.kind;
    header[1] = (block.data.length >> 16) & 0xff;
    header[2] = (block.data.length >> 8) & 0xff;
    header[3] = block.data.length & 0xff;
    parts.push(header, block.data);
  });
  parts.push(base.subarray(at));
  return Buffer.concat(parts);
}

// ---- Base JPEG and audio, then the injected fixtures ---------------------

function run(command, args) {
  const result = spawnSync(command, args, { stdio: 'inherit' });
  return !result.error && result.status === 0;
}

const convert2it = process.env.CONVERT2IT ?? 'convert2it';
if (!run(convert2it, ['--format', 'jpg', '--jpeg-quality', '90', '--output', out, '--overwrite', join(out, 'base.png')])) {
  console.error(`could not encode the base JPEG; set CONVERT2IT to the convert2it binary`);
  process.exit(1);
}
const exif = buildExif();
write(
  'metadata-photo.jpg',
  injectJpeg(readFileSync(join(out, 'base.jpg')), exif, buildIcc('convert2it JPEG')),
);
write(
  'metadata-photo.png',
  injectPng(basePng, exif, buildIcc('convert2it PNG')),
);

for (const format of ['mp3', 'flac', 'opus']) {
  if (!run(convert2it, ['--format', format, '--output', out, '--overwrite', join(out, 'base.wav')])) {
    console.error(`could not encode the base ${format}`);
    process.exit(1);
  }
}
write('metadata-tone.mp3', injectMp3(readFileSync(join(out, 'base.mp3'))));
write('metadata-tone.flac', injectFlac(readFileSync(join(out, 'base.flac'))));
write('metadata-tone.opus', readFileSync(join(out, 'base.opus')));

write('cover.png', coverPng());
const m4a = run('ffmpeg', [
  '-hide_banner', '-loglevel', 'error', '-y',
  '-i', join(out, 'base.wav'), '-i', join(out, 'cover.png'),
  '-map', '0:a', '-map', '1:v',
  '-c:a', 'aac', '-b:a', '192k', '-c:v', 'mjpeg', '-disposition:v', 'attached_pic',
  '-metadata', 'title=Waves', '-metadata', 'artist=Grüße',
  '-metadata', 'album=Metadata Album', '-metadata', 'album_artist=Album Artist',
  '-metadata', 'track=3', '-metadata', 'genre=Ambient', '-metadata', 'date=2026-11-24',
  '-metadata', 'comment=recorded for the metadata test',
  join(out, 'metadata-tone.m4a'),
]);
if (!m4a) console.error('ffmpeg is unavailable; metadata-tone.m4a was not regenerated');
const opus = run('ffmpeg', [
  '-hide_banner', '-loglevel', 'error', '-y',
  '-i', join(out, 'base.wav'), '-c:a', 'libopus', '-b:a', '96k',
  '-metadata:s:a:0', 'title=Waves', '-metadata:s:a:0', 'artist=Grüße',
  '-metadata:s:a:0', 'album=Metadata Album', '-metadata:s:a:0', 'track=3',
  '-metadata:s:a:0', 'genre=Ambient', '-metadata:s:a:0', 'date=2026-11-24',
  '-metadata:s:a:0', 'comment=recorded for the metadata test',
  join(out, 'metadata-tone.opus'),
]);
if (!opus) console.error('ffmpeg is unavailable; metadata-tone.opus was not regenerated');

console.log(`wrote the metadata fixtures into ${out}`);