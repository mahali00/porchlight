import { jsonSchema } from "../src/config.js";

await Bun.write(new URL("../porchlight.schema.json", import.meta.url), `${JSON.stringify(jsonSchema(), null, 2)}\n`);
