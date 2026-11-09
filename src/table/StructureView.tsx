import type { TableStructure } from "../api/types";

export function StructureView({ structure }: { structure: TableStructure }) {
  return (
    <div className="structure-view">
      <h3>Columns</h3>
      <table>
        <thead>
          <tr><th>Name</th><th>Type</th><th>Nullable</th><th>Default</th><th>PK</th></tr>
        </thead>
        <tbody>
          {structure.columns.map((c) => (
            <tr key={c.name}>
              <td>{c.name}</td>
              <td>{c.fullType}</td>
              <td>{c.nullable ? "yes" : "no"}</td>
              <td>{c.default ?? ""}</td>
              <td>{c.isPrimaryKey ? "✓" : ""}</td>
            </tr>
          ))}
        </tbody>
      </table>
      <h3>Indexes</h3>
      <table>
        <thead>
          <tr><th>Name</th><th>Columns</th><th>Unique</th><th>Primary</th></tr>
        </thead>
        <tbody>
          {structure.indexes.map((i) => (
            <tr key={i.name}>
              <td>{i.name}</td>
              <td>
                {i.columns.join(", ")}
                {i.included && i.included.length > 0 && (
                  <>
                    {" "}
                    <span className="index-include">INCLUDE ({i.included.join(", ")})</span>
                  </>
                )}
              </td>
              <td>{i.unique ? "✓" : ""}</td>
              <td>{i.primary ? "✓" : ""}</td>
            </tr>
          ))}
        </tbody>
      </table>
      <h3>Foreign keys</h3>
      <table>
        <thead>
          <tr><th>Name</th><th>Columns</th><th>References</th></tr>
        </thead>
        <tbody>
          {structure.foreignKeys.map((f) => (
            <tr key={f.name}>
              <td>{f.name}</td>
              <td>{f.columns.join(", ")}</td>
              <td>{`→ ${f.refTable.schema}.${f.refTable.name}(${f.refColumns.join(", ")})`}</td>
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}
