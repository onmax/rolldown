import {
  ACTIVE_SIZE,
  BLOCK_ALIGN,
  BLOCK_SIZE,
  BUFFER_SIZE,
  IS_TS_FLAG_POS,
  // @ts-ignore
} from 'oxc-parser/src-js/generated/constants.js';
// @ts-ignore
import { deserialize as deserializeJs } from 'oxc-parser/src-js/generated/deserialize/js.js';
// @ts-ignore
import { deserialize as deserializeTs } from 'oxc-parser/src-js/generated/deserialize/ts.js';
import * as binding from '../binding.cjs';
import type { ParseResult, ParserOptions } from './parse';

// Raw transfer: Rust writes the AST into an `ArrayBuffer` owned by JS, and JS builds
// the ESTree objects straight from that memory instead of from a JSON string. The
// binding already contains `oxc_parser_napi`'s raw transfer functions. The buffer
// layout is generated per Oxc version, so `oxc-parser` must stay pinned to the same
// version as the `oxc_parser_napi` crate.
const { getBufferOffset, parseRawSync } = binding as unknown as {
  getBufferOffset(buffer: Uint8Array): number;
  parseRawSync(
    filename: string,
    buffer: Uint8Array,
    sourceStart: number,
    sourceLen: number,
    options?: ParserOptions | null,
  ): void;
};

type RawBuffer = Uint8Array & { int32: Int32Array; float64: Float64Array; block: Uint8Array };

const ONE_GIB = 1 << 30;
const CLEAR_BUFFER_TIMEOUT = 10_000;
const textEncoder = new TextEncoder();

let supported: boolean | undefined;
let buffer: RawBuffer | undefined;
let oldBuffer: WeakRef<RawBuffer> | undefined;
let clearTimer: ReturnType<typeof setTimeout> | undefined;

function isSupported(): boolean {
  if (supported === undefined) {
    const major = globalThis.process?.release?.name === 'node' && !globalThis.process.versions.bun
      ? Number(process.versions.node.split('.')[0])
      : 0;
    supported = major >= 22 && typeof getBufferOffset === 'function' &&
      binding.rawTransferSupported();
  }
  return supported;
}

// A 2 GiB view aligned on 4 GiB inside a 6 GiB `ArrayBuffer`. Only pages that are
// written use physical memory. Windows charges the whole reservation against the
// commit limit, so the allocation can fail there; the caller then falls back to JSON.
function createBuffer(): RawBuffer {
  const arrayBuffer = new ArrayBuffer(BLOCK_SIZE + BLOCK_ALIGN);
  const offset = getBufferOffset(new Uint8Array(arrayBuffer));
  const buf = new Uint8Array(arrayBuffer, offset, BUFFER_SIZE) as RawBuffer;
  buf.int32 = new Int32Array(arrayBuffer, offset, BUFFER_SIZE / 4);
  buf.float64 = new Float64Array(arrayBuffer, offset, BUFFER_SIZE / 8);
  buf.block = new Uint8Array(arrayBuffer, offset, BLOCK_SIZE);
  return buf;
}

// `parseSync` never re-enters, so one buffer is enough. It is released to the
// garbage collector after 10 s without a parse.
function takeBuffer(): RawBuffer | undefined {
  if (clearTimer !== undefined) clearTimeout(clearTimer);
  clearTimer = undefined;
  buffer ??= oldBuffer?.deref();
  oldBuffer = undefined;
  if (buffer === undefined) {
    try {
      buffer = createBuffer();
    } catch {
      supported = false;
      return;
    }
  }
  return buffer;
}

function releaseBuffer(buf: RawBuffer): void {
  clearTimer = setTimeout(() => {
    clearTimer = undefined;
    oldBuffer = new WeakRef(buf);
    buffer = undefined;
  }, CLEAR_BUFFER_TIMEOUT);
  clearTimer.unref?.();
}

/**
 * Parse with raw transfer. Returns `undefined` when raw transfer can't be used
 * (platform, runtime, failed allocation, or options the default deserializers
 * don't cover), so the caller can use the JSON path.
 */
export function parseSyncRaw(
  filename: string,
  sourceText: string,
  options?: ParserOptions | null,
): ParseResult | undefined {
  if (options?.range || !isSupported()) return;
  const maxSourceByteLen = sourceText.length * 3;
  if (maxSourceByteLen > ONE_GIB) return;
  const buf = takeBuffer();
  if (buf === undefined) return;
  try {
    const sourceStart = ACTIVE_SIZE - maxSourceByteLen;
    const { read, written: sourceLen } = textEncoder.encodeInto(
      sourceText,
      new Uint8Array(buf.buffer, buf.byteOffset + sourceStart, maxSourceByteLen),
    );
    if (read !== sourceText.length) return;
    parseRawSync(filename, buf.block, sourceStart, sourceLen, options);
    const isJs = buf[IS_TS_FLAG_POS] === 0;
    const data = (isJs ? deserializeJs : deserializeTs)(buf, sourceText, sourceStart, sourceLen);
    // The JSON path adds a line comment for a hashbang in JS files; do the same.
    if (isJs && data.program.hashbang !== null) {
      const { value, start, end } = data.program.hashbang;
      data.comments.unshift({ type: 'Line', value, start, end });
    }
    return {
      get program() {
        return data.program;
      },
      get module() {
        return data.module;
      },
      get comments() {
        return data.comments;
      },
      get errors() {
        return data.errors;
      },
    } as ParseResult;
  } finally {
    releaseBuffer(buf);
  }
}
