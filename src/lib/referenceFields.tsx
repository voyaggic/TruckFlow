// Shared reference-field definitions + entity display names for the whole app.
// Every screen reads labels from here instead of hardcoding
// "Plate / Company / Driver…" or "Vehicles / Companies / Drivers", so
// renaming a field or entity in Admin → Fields propagates everywhere.
import { createContext, useCallback, useContext, useEffect, useState, type ReactNode } from "react";
import type { FieldDefinition, ReferenceEntity, ReferenceEntityType } from "./types";
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
  /** All parent entities (Vehicles/Companies/Drivers plus admin-added ones). */
  entities: ReferenceEntity[];
  fields: Record<ReferenceEntityType, FieldDefinition[]>;
  /** Field definitions for any parent (core or admin-added), keyed by entity type. */
  fieldsFor: (entityType: string) => FieldDefinition[];
  /** Label for a field given its binding (standard) or key (custom). */
  label: (entity: ReferenceEntityType, key: string) => string;
  /** Display name for a parent entity (e.g. "Vehicles" or "Trailers"). */
  entityLabel: (entity: string) => string;
  refresh: () => Promise<void>;
}

const ReferenceFieldsContext = createContext<ReferenceFieldsValue>({
  entities: [],
  fields: { vehicle: [], company: [], driver: [] },
  fieldsFor: () => [],
  label: (_entity, key) => DEFAULT_LABELS[key] ?? key,
  entityLabel: (entity) => DEFAULT_ENTITY_LABELS[entity as ReferenceEntityType] ?? entity,
  refresh: async () => {},
});

export function ReferenceFieldsProvider({ children }: { children: ReactNode }) {
  const [entities, setEntities] = useState<ReferenceEntity[]>([]);
  const [fields, setFields] = useState<Record<ReferenceEntityType, FieldDefinition[]>>({
    vehicle: [],
    company: [],
    driver: [],
  });
  const [allFields, setAllFields] = useState<Record<string, FieldDefinition[]>>({});

  const refresh = useCallback(async () => {
    try {
      const [ents, vf, cf, df] = await Promise.all([
        api.listReferenceEntities(),
        api.listFieldDefinitions("vehicle"),
        api.listFieldDefinitions("company"),
        api.listFieldDefinitions("driver"),
      ]);
      setEntities(ents);
      setFields({ vehicle: vf, company: cf, driver: df });
      // Load field definitions for every parent (incl. admin-added ones).
      const extra: Record<string, FieldDefinition[]> = {};
      await Promise.all(
        ents
          .filter((e) => e.entity_type !== "vehicle" && e.entity_type !== "company" && e.entity_type !== "driver")
          .map(async (e) => {
            try {
              extra[e.entity_type] = await api.listFieldDefinitions(e.entity_type);
            } catch {
              extra[e.entity_type] = [];
            }
          }),
      );
      setAllFields(extra);
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
    (entity: string) => {
      const e = entities.find((x) => x.entity_type === entity);
      if (e && e.label) return e.label;
      return DEFAULT_ENTITY_LABELS[entity as ReferenceEntityType] ?? entity;
    },
    [entities],
  );

  const fieldsFor = useCallback(
    (entityType: string) => {
      if (entityType === "vehicle" || entityType === "company" || entityType === "driver") {
        return fields[entityType as ReferenceEntityType] ?? [];
      }
      return allFields[entityType] ?? [];
    },
    [fields, allFields],
  );

  return (
    <ReferenceFieldsContext.Provider value={{ entities, fields, fieldsFor, label, entityLabel, refresh }}>
      {children}
    </ReferenceFieldsContext.Provider>
  );
}

export function useReferenceFields() {
  return useContext(ReferenceFieldsContext);
}
