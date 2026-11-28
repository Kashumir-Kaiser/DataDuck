// Only connect to the loopback Analysis Services engine started for the synthetic Desktop project.
using Microsoft.AnalysisServices.AdomdClient;
using Microsoft.AnalysisServices.Tabular;

if (args.Length != 2 || !int.TryParse(args[0], out var port) || port < 1 || port > 65535
    || (args[1] != "refresh" && args[1] != "verify"))
    throw new ArgumentException("Pass <Desktop engine port> and refresh or verify.");
string address = $"Data Source=localhost:{port};Connect Timeout=10";
using var server = new Server();
server.Connect(address);
var db = server.Databases.Cast<Database>().Single(d => d.Model?.Expressions.Find("DataDuckSnapshotFolder") != null);
var model = db.Model;
var fact = model.Tables.Single(t => t.Columns.Find("Selling_Price") != null && t.Columns.Find("Make") != null);
if (args[1] == "refresh")
{
    model.RequestRefresh(RefreshType.Full);
    model.SaveChanges();
}
using var connection = new AdomdConnection(address + ";Initial Catalog=" + db.ID);
connection.Open();
using var command = connection.CreateCommand();
string table = "'" + fact.Name.Replace("'", "''") + "'";
command.CommandText = $"EVALUATE ROW(\"Total\",COUNTROWS({table}),\"Ford\",CALCULATE(COUNTROWS({table}),{table}[Make]=\"Ford\"),\"FordMean\",CALCULATE(AVERAGE({table}[Selling_Price]),{table}[Make]=\"Ford\"))";
using var reader = command.ExecuteReader();
if (!reader.Read() || Convert.ToInt32(reader[0]) != 16 || Convert.ToInt32(reader[1]) != 8 || Convert.ToDecimal(reader[2]) != 170m)
    throw new InvalidDataException("The refreshed Desktop model returned incorrect counts or average selling price.");
Console.WriteLine("Desktop's live engine returned 16 rows, 8 Ford rows and average Ford price 170.");
