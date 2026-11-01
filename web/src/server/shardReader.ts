import fs from "node:fs/promises";
import path from "node:path";
import { decompress } from "fzstd";

export type WorkLocation = {
  shard: string;
  offset: number;
  length: number;
};

type FrameSpan = {
  compressedStart: number;
  compressedLength: number;
  decompressedStart: number;
  decompressedLength: number;
};

type Shard = {
  file: string;
  frames: readonly FrameSpan[];
};

export type ShardIndex = {
  shards: ReadonlyMap<string, Shard>;
  works: ReadonlyMap<number, WorkLocation>;
};

// from zeekstd
// i may just be going crazy
const SEEKABLE_MAGIC = 0x8f92_eab1;
const SKIPPABLE_MAGIC_MASK = 0xffff_fff0;
const SKIPPABLE_MAGIC_BASE = 0x184d_2a50;
const TABLE_HEADER_SIZE = 8;
const ENTRY_SIZE = 8;
const INTEGRITY_SIZE = 9;
const IDX_RECORD_SIZE = 12;
const FRAME_CACHE_LIMIT = 4;
const TRAILER_SEARCH_BYTES = 1 << 20;

const frameCache = new Map<string, Uint8Array>();

// TODO: uh make this work in prod or something idk
const dataDir = () => process.env.AO3_DATA_DIR ?? path.resolve(process.cwd(), "../output94");

const readAt = async (file: string, position: number, length: number): Promise<Buffer> => {
  const handle = await fs.open(file, "r");
  try {
    const buffer = Buffer.alloc(length);
    const { bytesRead } = await handle.read(buffer, 0, length, position);
    if (bytesRead !== length) {
      throw new Error(`${path.basename(file)}: short read at ${position} (${bytesRead}/${length})`);
    }
    return buffer;
  } finally {
    await handle.close();
  }
};

const seekableHeaderFor = (file: string, size: number, tail: Buffer, tailStart: number): number => {
  for (let position = tail.length - TABLE_HEADER_SIZE; position >= TABLE_HEADER_SIZE; position--) {
    if ((tail.readUInt32LE(position) & SKIPPABLE_MAGIC_MASK) !== SKIPPABLE_MAGIC_BASE) continue;
    const declaredSize = tail.readUInt32LE(position + 4);
    if (declaredSize + TABLE_HEADER_SIZE !== size - tailStart - position) continue;
    return tailStart + position;
  }
  throw new Error(`${path.basename(file)}: no seek table header in the last ${tail.length} bytes`);
};

const framesFor = async (file: string, size: number): Promise<FrameSpan[]> => {
  const tailLength = Math.min(size, TRAILER_SEARCH_BYTES);
  const tailStart = size - tailLength;
  const tail = await readAt(file, tailStart, tailLength);
  const tableStart = seekableHeaderFor(file, size, tail, tailStart);

  const tableEnd = tail.length - INTEGRITY_SIZE;
  const trailingMagic = tail.readUInt32LE(tableEnd + 5);
  if (trailingMagic !== SEEKABLE_MAGIC) {
    throw new Error(
      `${path.basename(file)}: trailing magic ${trailingMagic.toString(16)}, expected ${SEEKABLE_MAGIC.toString(16)}`,
    );
  }
  const count = tail.readUInt32LE(tableEnd);
  const entryBytes = count * ENTRY_SIZE;
  if (tableStart + TABLE_HEADER_SIZE + entryBytes + INTEGRITY_SIZE !== size) {
    throw new Error(`${path.basename(file)}: ${count} entries do not reach the trailer from ${tableStart}`);
  }

  const table = await readAt(file, tableStart + TABLE_HEADER_SIZE, entryBytes);
  const frames: FrameSpan[] = [];
  let compressedPosition = 0;
  let decompressedPosition = 0;
  for (let index = 0; index < count; index++) {
    const compressedLength = table.readUInt32LE(index * ENTRY_SIZE);
    const decompressedLength = table.readUInt32LE(index * ENTRY_SIZE + 4);
    frames.push({
      compressedStart: compressedPosition,
      compressedLength,
      decompressedStart: decompressedPosition,
      decompressedLength,
    });
    compressedPosition += compressedLength;
    decompressedPosition += decompressedLength;
  }

  if (compressedPosition !== tableStart) {
    throw new Error(`${path.basename(file)}: frames span ${compressedPosition}, seek table starts at ${tableStart}`);
  }
  return frames;
};

const worksFor = async (idxFile: string, shard: string): Promise<[number, WorkLocation][]> => {
  const data = await fs.readFile(idxFile);
  if (data.length % IDX_RECORD_SIZE !== 0) {
    throw new Error(`${path.basename(idxFile)}: ${data.length} bytes is not a multiple of ${IDX_RECORD_SIZE}`);
  }
  const works: [number, WorkLocation][] = [];
  for (let position = 0; position < data.length; position += IDX_RECORD_SIZE) {
    works.push([
      data.readUInt32LE(position),
      { shard, offset: data.readUInt32LE(position + 4), length: data.readUInt32LE(position + 8) },
    ]);
  }
  return works;
};

type LoadedShard = {
  name: string;
  shard: Shard;
  works: [number, WorkLocation][];
};

const loadShard = async (idxFile: string): Promise<LoadedShard> => {
  const name = path.basename(idxFile, ".idx");
  const file = idxFile.replace(/\.idx$/, ".seekable.zst");
  const { size } = await fs.stat(file);
  return { name, shard: { file, frames: await framesFor(file, size) }, works: await worksFor(idxFile, name) };
};

let indexPromise: Promise<ShardIndex> | undefined;

const loadIndex = async (): Promise<ShardIndex> => {
  const dir = dataDir();
  const idxFiles = (await fs.readdir(dir))
    .filter((entry) => entry.endsWith(".idx"))
    .sort()
    .map((entry) => path.join(dir, entry));
  const loaded = await Promise.all(idxFiles.map(loadShard));
  return {
    shards: new Map(loaded.map(({ name, shard }) => [name, shard])),
    works: new Map(loaded.flatMap(({ works }) => works)),
  };
};

export const shardIndex = (): Promise<ShardIndex> => {
  indexPromise ??= loadIndex().catch((error: unknown) => {
    indexPromise = undefined;
    throw error;
  });
  return indexPromise;
};

const frameOf = (location: WorkLocation, frames: readonly FrameSpan[]): FrameSpan | undefined => {
  const end = location.offset + location.length;
  return frames.find(
    (frame) => location.offset >= frame.decompressedStart && end <= frame.decompressedStart + frame.decompressedLength,
  );
};

const remember = (key: string, frame: Uint8Array): Uint8Array => {
  frameCache.delete(key);
  frameCache.set(key, frame);
  for (const oldest of frameCache.keys()) {
    if (frameCache.size <= FRAME_CACHE_LIMIT) break;
    frameCache.delete(oldest);
  }
  return frame;
};

const decompressedFrame = async (file: string, frame: FrameSpan): Promise<Uint8Array> => {
  const key = `${file}:${frame.compressedStart}`;
  const cached = frameCache.get(key);
  if (cached) return remember(key, cached);
  const compressed = await readAt(file, frame.compressedStart, frame.compressedLength);
  const data = decompress(new Uint8Array(compressed));
  if (data.length !== frame.decompressedLength) {
    throw new Error(
      `${path.basename(file)}@${frame.compressedStart}: decompressed ${data.length}, table says ${frame.decompressedLength}`,
    );
  }
  return remember(key, data);
};

export const readWorkText = async (workId: number): Promise<string | undefined> => {
  const index = await shardIndex();
  const location = index.works.get(workId);
  if (!location) return undefined;
  const shard = index.shards.get(location.shard);
  if (!shard) throw new Error(`work ${workId}: unknown shard ${location.shard}`);
  const frame = frameOf(location, shard.frames);
  if (!frame) {
    throw new Error(`work ${workId}: bytes ${location.offset}..${location.offset + location.length} straddle frames`);
  }
  const data = await decompressedFrame(shard.file, frame);
  const start = location.offset - frame.decompressedStart;
  return new TextDecoder().decode(data.subarray(start, start + location.length));
};
