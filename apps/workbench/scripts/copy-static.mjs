// tsc 只负责编译 TS；静态入口与样式由这个脚本复制，避免引入打包器。
import { copyFileSync, mkdirSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const root = dirname(fileURLToPath(import.meta.url));
const project = join(root, "..");
const dist = join(project, "dist");

mkdirSync(dist, { recursive: true });
copyFileSync(join(project, "index.html"), join(dist, "index.html"));
console.log("静态资源已复制到 dist/");
