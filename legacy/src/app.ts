import packageJson from "../package.json" with { type: "json" };
import { createCli } from "./engine/index.ts";
import { schedulesModule } from "./modules/schedules/module.ts";
import { systemModule } from "./modules/system/module.ts";

export const app = createCli({
  meta: {
    description: "Gated schedules and workflows for invoking AI",
    homepage: "https://github.com/aaronvanston/ultradian",
    name: "ultradian",
    version: packageJson.version,
  },
  modules: [schedulesModule, systemModule],
});
