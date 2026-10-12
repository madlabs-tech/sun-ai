// After `tsc`: turn dist/ into the npm package (`npm publish dist`). The repo
// copy runs src/index.ts directly (type stripping), but Node refuses to strip
// types under node_modules, so the published package is compiled JS + .d.ts.
import { copyFileSync, readFileSync, writeFileSync } from "node:fs";

const package_json = JSON.parse(readFileSync("package.json", "utf8"));
delete package_json.scripts;
delete package_json.devDependencies;
package_json.exports = { types: "./index.d.ts", default: "./index.js" };
package_json.files = ["index.js", "index.d.ts", "LICENSE"];
writeFileSync("dist/package.json", JSON.stringify(package_json, null, 2) + "\n");
copyFileSync("LICENSE", "dist/LICENSE");
