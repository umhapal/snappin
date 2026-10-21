#!/usr/bin/env node
// 构建 SnapPin OCR 引擎包：onnxruntime.dll + PP-OCRv5 mobile 检测/识别模型 + 字典。
//
// 为什么需要这个脚本：README 承诺 Releases 上有引擎包，但模型与 DLL 都不在本仓库里
// （模型约 22 MB、DLL 12 MB，进 git 会把仓库撑大且难以审计）。这里从各自上游拉取并
// 校验 sha256，产出一个可复现的 zip——谁跑都得到同一份字节。
//
// 来源与许可：
//   - onnxruntime.dll  来自 microsoft/onnxruntime 官方 release v1.22.0（MIT）
//   - PP-OCRv5 模型     来自 modelscope.cn/models/greatv/oar-ocr（Apache-2.0）
//
// 用法：
//   node scripts/build-ocr-engine.mjs [--out <路径>] [--skip-dll]
//
// 无参数时输出到 <out>/snappin-ocr-engine-v<version>.zip。

import { createHash } from "node:crypto";
import { createWriteStream } from "node:fs";
import { mkdir, readFile, readdir, rm, stat, writeFile } from "node:fs/promises";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { execFileSync } from "node:child_process";

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), "..");

const ORT_VERSION = "1.22.0";
const ORT_URL = `https://github.com/microsoft/onnxruntime/releases/download/v${ORT_VERSION}/onnxruntime-win-x64-${ORT_VERSION}.zip`;
const ORT_DLL_SHA256 = "579b636403983254346a5c1d80bd28f1519cd1e284cd204f8d4ff41f8d711559";

const MODELSCOPE_REPO = "greatv/oar-ocr";
const MODELSCOPE_REVISION = "master";
const MODELSCOPE_URL = (file) =>
  `https://www.modelscope.cn/api/v1/models/${MODELSCOPE_REPO}/repo?Revision=${MODELSCOPE_REVISION}&FilePath=${file}`;

// 三个模型文件：名字取自 oar-ocr 的 bare 名（src-tauri/src/ocr.rs 的 DET/REC/DICT 常量），
// sha256 与 oar-ocr-core 0.9.2 的下载白名单一致（registry.rs 的 Entry 行）。
const MODELS = [
  {
    name: "pp-ocrv5_mobile_det.onnx",
    sha256: "1eb7b4f7ab657ebd1c66d5f79bca7497f29768a2e3c15e52daecbba1a8e4a039",
    size: 4826518,
  },
  {
    name: "pp-ocrv5_mobile_rec.onnx",
    sha256: "243a0f06d826761323e9045e9b113ab2c191c3aa50565585e628300b8eda0224",
    size: 16562373,
  },
  {
    name: "ppocrv5_dict.txt",
    sha256: "d1979e9f794c464c0d2e0b70a7fe14dd978e9dc644c0e71f14158cdf8342af1b",
    size: 74012,
  },
];

const THIRD_PARTY_LICENSES = `Third-party components redistributed in this archive
====================================================

This archive contains the SnapPin OCR engine: the ONNX Runtime shared library
plus PP-OCRv5 mobile detection/recognition models and their dictionary.

1) onnxruntime.dll
   ONNX Runtime, by Microsoft Corporation
   License: MIT
   https://github.com/microsoft/onnxruntime
   Copyright (c) Microsoft Corporation

   Permission is hereby granted, free of charge, to any person obtaining a copy
   of this software and associated documentation files (the "Software"), to deal
   in the Software without restriction, including without limitation the rights
   to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
   copies of the Software, and to permit persons to whom the Software is
   furnished to do so, subject to the following conditions:

   The above copyright notice and this permission notice shall be included in all
   copies or substantial portions of the Software.

   THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
   IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
   FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
   AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
   LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
   OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
   SOFTWARE.

2) pp-ocrv5_mobile_det.onnx, pp-ocrv5_mobile_rec.onnx, ppocrv5_dict.txt
   PP-OCRv5, by PaddlePaddle / Baidu
   License: Apache License 2.0
   https://github.com/PaddlePaddle/PaddleOCR

   Licensed under the Apache License, Version 2.0 (the "License");
   you may not use this file except in compliance with the License.
   You may obtain a copy of the License at

       http://www.apache.org/licenses/LICENSE-2.0

   Unless required by applicable law or agreed to in writing, software
   distributed under the License is distributed on an "AS IS" BASIS,
   WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
   See the License for the specific language governing permissions and
   limitations under the License.

3) The *.sha256 files are checksum sidecars written by the oar-ocr
   auto-download cache; they are not part of any third-party distribution.

Files in this archive
---------------------
  onnxruntime.dll              ONNX Runtime shared library (MIT)
  pp-ocrv5_mobile_det.onnx     PP-OCRv5 mobile text detection model (Apache-2.0)
  pp-ocrv5_mobile_rec.onnx     PP-OCRv5 mobile text recognition model (Apache-2.0)
  ppocrv5_dict.txt             PP-OCRv5 recognition dictionary (Apache-2.0)
  .*.sha256                    checksum sidecars for the three files above

How to use
----------
Unzip so that onnxruntime.dll and the .onnx files end up in engines/oar-ocr/
next to snappin.exe (or in %LOCALAPPDATA%\\snappin\\engines\\oar-ocr\\, or point
SNAPPIN_OCR_ENGINE_DIR at the folder). SnapPin then runs OCR fully offline.
`;

const args = process.argv.slice(2);
const outIdx = args.indexOf("--out");
const OUT_DIR = outIdx >= 0 ? resolve(args[outIdx + 1]) : join(ROOT, "release-assets");
const SKIP_DLL = args.includes("--skip-dll");

async function sha256OfFile(path) {
  const buf = await readFile(path);
  return createHash("sha256").update(buf).digest("hex");
}

async function download(url, dest) {
  const res = await fetch(url, { redirect: "follow" });
  if (!res.ok) throw new Error(`下载失败 ${res.status}: ${url}`);
  await mkdir(dirname(dest), { recursive: true });
  const out = createWriteStream(dest);
  for await (const chunk of res.body) out.write(chunk);
  await new Promise((r, j) => {
    out.end(r);
    out.on("error", j);
  });
}

async function fetchModels(stage) {
  for (const m of MODELS) {
    const dest = join(stage, m.name);
    await download(MODELSCOPE_URL(m.name), dest);
    const got = await sha256OfFile(dest);
    const size = (await stat(dest)).size;
    if (got !== m.sha256 || size !== m.size) {
      throw new Error(`${m.name} 校验失败：sha256=${got} size=${size}，期望 sha256=${m.sha256} size=${m.size}`);
    }
    console.log(`模型 OK  ${m.name}  ${size} 字节`);
    // sidecar 让 oar-ocr 的缓存判定免去重算哈希（它认这个文件名格式）。
    await writeFile(join(stage, `.${m.name}.sha256`), m.sha256);
  }
}

async function fetchDll(stage) {
  const zipPath = join(stage, "onnxruntime.zip");
  console.log(`拉取 ONNX Runtime ${ORT_VERSION}…`);
  await download(ORT_URL, zipPath);
  const extracted = join(stage, "ort");
  mkdir(extracted, { recursive: true });
  // Windows 自带 Expand-Archive；非 Windows 用 unzip 兜底。
  if (process.platform === "win32") {
    execFileSync("powershell", [
      "-NoProfile",
      "-Command",
      `Expand-Archive -LiteralPath '${zipPath}' -DestinationPath '${extracted}' -Force`,
    ]);
  } else {
    execFileSync("unzip", ["-q", zipPath, "-d", extracted]);
  }
  const entries = [];
  const walk = async (dir) => {
    for (const name of await readdir(dir, { withFileTypes: true })) {
      const p = join(dir, name.name);
      if (name.isDirectory()) await walk(p);
      else if (name.name === "onnxruntime.dll") entries.push(p);
    }
  };
  await walk(extracted);
  if (entries.length === 0) throw new Error("ORT 包里没有 onnxruntime.dll");
  const dllSrc = entries[0];
  const dllDest = join(stage, "onnxruntime.dll");
  const got = await sha256OfFile(dllSrc);
  if (got !== ORT_DLL_SHA256) throw new Error(`onnxruntime.dll 校验失败：${got}`);
  await writeFile(dllDest, await readFile(dllSrc));
  console.log(`DLL OK    onnxruntime.dll  ${(await stat(dllDest)).size} 字节`);
  await rm(zipPath, { force: true });
  await rm(extracted, { recursive: true, force: true });
}

async function makeZip(stage, outFile) {
  // Windows 用 Compress-Archive；其余用 zip。注意 Compress-Archive 会带目录结构，
  // 所以先把要打的文件平铺到一个 staging 目录再压。
  if (process.platform === "win32") {
    execFileSync("powershell", [
      "-NoProfile",
      "-Command",
      `Compress-Archive -Path '${join(stage, "*")}' -DestinationPath '${outFile}' -Force`,
    ]);
  } else {
    execFileSync("zip", ["-r", "-q", outFile, ".", "-x", ".*"], { cwd: stage });
  }
}

const version = JSON.parse(await readFile(join(ROOT, "src-tauri", "tauri.conf.json"), "utf8")).version;
const stage = join(OUT_DIR, ".ocr-engine-stage");
await rm(stage, { recursive: true, force: true });
await mkdir(stage, { recursive: true });

if (!SKIP_DLL) await fetchDll(stage);
await fetchModels(stage);
await writeFile(join(stage, "THIRD_PARTY_LICENSES.txt"), THIRD_PARTY_LICENSES);

await mkdir(OUT_DIR, { recursive: true });
const outFile = join(OUT_DIR, `snappin-ocr-engine-v${version}.zip`);
await rm(outFile, { force: true });
await makeZip(stage, outFile);
await rm(stage, { recursive: true, force: true });
console.log(`引擎包已生成：${outFile}  ${(await stat(outFile)).size} 字节`);
