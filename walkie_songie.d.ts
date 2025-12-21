/* tslint:disable */
/* eslint-disable */

/**
 * Run the web application.
 */
export function run_app(): void;

export type InitInput = RequestInfo | URL | Response | BufferSource | WebAssembly.Module;

export interface InitOutput {
  readonly memory: WebAssembly.Memory;
  readonly run_app: () => void;
  readonly __wasm_bindgen_func_elem_1793: (a: number, b: number, c: number) => void;
  readonly __wasm_bindgen_func_elem_337: (a: number, b: number) => void;
  readonly __wasm_bindgen_func_elem_4378: (a: number, b: number, c: number) => void;
  readonly __wasm_bindgen_func_elem_4354: (a: number, b: number) => void;
  readonly __wasm_bindgen_func_elem_1792: (a: number, b: number, c: number) => void;
  readonly __wasm_bindgen_func_elem_5001: (a: number, b: number, c: number) => void;
  readonly __wasm_bindgen_func_elem_4789: (a: number, b: number) => void;
  readonly __wasm_bindgen_func_elem_5480: (a: number, b: number) => void;
  readonly __wasm_bindgen_func_elem_5463: (a: number, b: number) => void;
  readonly __wasm_bindgen_func_elem_1791: (a: number, b: number) => void;
  readonly __wasm_bindgen_func_elem_9892: (a: number, b: number, c: number) => void;
  readonly __wasm_bindgen_func_elem_9879: (a: number, b: number) => void;
  readonly __wasm_bindgen_func_elem_8782: (a: number, b: number) => void;
  readonly __wasm_bindgen_func_elem_8766: (a: number, b: number) => void;
  readonly __wasm_bindgen_func_elem_10017: (a: number, b: number, c: number) => void;
  readonly __wasm_bindgen_func_elem_10002: (a: number, b: number) => void;
  readonly __wasm_bindgen_func_elem_13509: (a: number, b: number, c: number, d: number) => void;
  readonly __wbindgen_export: (a: number, b: number) => number;
  readonly __wbindgen_export2: (a: number, b: number, c: number, d: number) => number;
  readonly __wbindgen_export3: (a: number) => void;
  readonly __wbindgen_export4: (a: number, b: number, c: number) => void;
  readonly __wbindgen_start: () => void;
}

export type SyncInitInput = BufferSource | WebAssembly.Module;

/**
* Instantiates the given `module`, which can either be bytes or
* a precompiled `WebAssembly.Module`.
*
* @param {{ module: SyncInitInput }} module - Passing `SyncInitInput` directly is deprecated.
*
* @returns {InitOutput}
*/
export function initSync(module: { module: SyncInitInput } | SyncInitInput): InitOutput;

/**
* If `module_or_path` is {RequestInfo} or {URL}, makes a request and
* for everything else, calls `WebAssembly.instantiate` directly.
*
* @param {{ module_or_path: InitInput | Promise<InitInput> }} module_or_path - Passing `InitInput` directly is deprecated.
*
* @returns {Promise<InitOutput>}
*/
export default function __wbg_init (module_or_path?: { module_or_path: InitInput | Promise<InitInput> } | InitInput | Promise<InitInput>): Promise<InitOutput>;
