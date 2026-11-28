// Validate a generated project with the same Tabular Object Model used by Desktop.
using Microsoft.AnalysisServices.Tabular;
using System.Text.Json;

if (args.Length != 1)
    throw new ArgumentException("Pass the exported PBIP project directory.");

var root = Path.GetFullPath(args[0]);
var semantic = Directory.GetDirectories(root, "*.SemanticModel").Single();
var database = TmdlSerializer.DeserializeDatabaseFromFolder(Path.Combine(semantic, "definition"));
var model = database.Model;
var report = Directory.GetDirectories(root, "*.Report").Single();

foreach (var relationship in model.Relationships.OfType<SingleColumnRelationship>())
{
    if (relationship.FromColumn is null || relationship.ToColumn is null ||
        relationship.FromColumn.DataType != relationship.ToColumn.DataType)
        throw new InvalidDataException($"Invalid relationship {relationship.Name}");
}

var files = Directory.GetFiles(report, "visual.json", SearchOption.AllDirectories);
if (files.Length == 0)
    throw new InvalidDataException("No PBIR visuals found.");
int bindings = 0;
foreach (string file in files)
{
    using var document = JsonDocument.Parse(File.ReadAllText(file));
    Check(document.RootElement, file);
}
if (bindings == 0)
    throw new InvalidDataException("No tabular field bindings found.");
Console.WriteLine($"TOM loaded {model.Tables.Count} tables; resolved {bindings} bindings in {files.Length} visuals.");

void Check(JsonElement node, string file)
{
    if (node.ValueKind == JsonValueKind.Array)
    {
        foreach (var element in node.EnumerateArray()) Check(element, file);
        return;
    }
    if (node.ValueKind != JsonValueKind.Object) return;
    foreach (var member in node.EnumerateObject())
    {
        if (member.Name is "Column" or "Measure" &&
            member.Value.TryGetProperty("Expression", out var expr) &&
            expr.TryGetProperty("SourceRef", out var source) &&
            source.TryGetProperty("Entity", out var entity))
        {
            var name = member.Value.GetProperty("Property").GetString();
            var table = model.Tables.FirstOrDefault(t => t.Name == entity.GetString());
            bool resolved = member.Name == "Column"
                ? table?.Columns.Any(c => c.Name == name) == true
                : table?.Measures.Any(m => m.Name == name) == true;
            if (!resolved)
                throw new InvalidDataException($"Unresolved {member.Name} {entity.GetString()}.{name} in {file}");
            bindings++;
        }
        Check(member.Value, file);
    }
}
