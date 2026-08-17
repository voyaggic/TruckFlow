// Shared reference-field definitions + entity display names for the whole app.
// Every screen reads labels from here instead of hardcoding
// "Plate / Company / Driver…" or "Vehicles / Companies / Drivers", so
// renaming a field or entity in Admin → Fields propagates everywhere.
import { createContext, useCallback, useContext, useEffect, useState, type ReactNode } from "react";
import type { EntityLabels, FieldDefinition, ReferenceEntityType } from "./types";
import { DEFAULT_ENTITY_LABELS } from "./types";
import { api } from "./api";

/** Fallback labels used when a field definition isn't loaded (or was deleted). */
const DEFAULT_LABELS: Record<string, string> = {
  plate_number: "Plate",
  company: "Company",
  driver: "Driver",
  registered_capacity: "Capacity",
  capacity_unit: "Capacity Unit",
  status: "Status",
  name: "Name",
};

interface ReferenceFieldsValue {
  fields: Record<ReferenceEntityType, FieldDefinition[]>;
  /** Label for a field given its binding (standard) or key (custom). */
  label: (entity: ReferenceEntityType, key: string) => string;
  /** Display name for an entity (Vehicles / Companies / Drivers). */
  entityLabel: (entity: ReferenceEntityType) => string;
  refresh: () => Promise<void>;
}

const ReferenceFieldsContext = createContext<ReferenceFieldsValue>({
  fields: { vehicle: [], company: [], driver: [] },
  label: (_entity, key) => DEFAULT_LABELS[key] ?? key,
  entityLabel: (entity) => DEFAULT_ENTITY_LABELS[entity],
  refresh: async () => {},
});

export function ReferenceFieldsProvider({ children }: { children: ReactNode }) {
  const [fields, setFields] = useState<Record<ReferenceEntityType, FieldDefinition[]>>({
    vehicle: [],
    company: [],
    driver: [],
  });
  const [entityLabels, setEntityLabels] = useState<EntityLabels>({ ...DEFAULT_ENTITY_LABELS });

  const refresh = useCallback(async () => {
    try {
      const [vf, cf, df, labels] = await Promise.all([
        api.listFieldDefinitions("vehicle"),
        api.listFieldDefinitions("company"),
        api.listFieldDefinitions("driver"),
        api.listEntityLabels(),
      ]);
      setFields({ vehicle: vf, company: cf, driver: df });
      setEntityLabels({ ...DEFAULT_ENTITY_LABELS, ...labels });
    } catch {
      // Fall back to default labels until the next successful load.
    }
  }, []);

  useEffect(() => {
    refresh();
  }, [refresh]);

  const label = useCallback(
    (entity: ReferenceEntityType, key: string) => {
      const f = fields[entity].find((fd) => (fd.is_standard ? fd.binding === key : fd.field_key === key));
      if (f && f.field_label) return f.field_label;
      return DEFAULT_LABELS[key] ?? key;
    },
    [fields],
  );

  const entityLabel = useCallback(
    (entity: ReferenceEntityType) => entityLabels[entity] ?? DEFAULT_ENTITY_LABELS[entity],
    [entityLabels],
  );

  return (
    <ReferenceFieldsContext.Provider value={{ fields, label, entityLabel, refresh }}>
      {children}
    </ReferenceFieldsContext.Provider>
  );
}

export function useReferenceFields() {
  return useContext(ReferenceFieldsContext);
}
