import {
  formatServiceConfig,
  normalizeServiceConfig,
  toPersistedServiceConfig,
  type ServiceConfigInput,
} from "../service.js";
import { redactSecrets } from "./shared.js";

export interface PrintConfigOptions {
  config: ServiceConfigInput;
  redact?: boolean;
}

export interface PrintConfigResult {
  config: ReturnType<typeof toPersistedServiceConfig>;
  text: string;
}

export function printConfig(options: PrintConfigOptions): PrintConfigResult {
  const config = toPersistedServiceConfig(normalizeServiceConfig(options.config));
  const value = options.redact === false ? config : (redactSecrets(config) as typeof config);
  return {
    config,
    text: formatServiceConfig(value as typeof config),
  };
}
