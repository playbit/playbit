import Foundation

@main enum CuratedExampleTests {
    static func entry(_ id: String = "shared-todo") -> [String: Any] {
        ["id": id, "name": "Shared TODO", "description": "Everyone with the example can read and edit these tasks.",
         "invitation": "https://playbit.app/s#RDJVpDon9tg5g3s4dkADuL",
         "source": "https://github.com/playbit/playbit/tree/v0.1.3/guests/todo"]
    }
    static func reject(_ value: Any?) {
        do { _ = try CuratedExample.parse(value); preconditionFailure("accepted invalid examples") }
        catch { precondition(error.localizedDescription.contains("saved session")) }
    }
    static func main() throws {
        let empty = try CuratedExample.parse([])
        precondition(empty.isEmpty)
        let examples = try CuratedExample.parse([entry()])
        precondition(examples[0].name == "Shared TODO")
        precondition(examples[0].description.contains("read and edit"))
        precondition(examples[0].source.absoluteString == entry()["source"] as? String)
        reject(nil); reject([entry(), entry()])
        reject((0..<5).map { entry("example-\($0)") })
        for (field, value) in [("name", ""), ("description", String(repeating: "é", count: 257)),
                               ("invitation", "https://other.invalid/s#RDJVpDon9tg5g3s4dkADuL"),
                               ("invitation", "https://playbit.app/s#0000000000000000000000"),
                               ("source", "file:///private/profile"), ("source", "https://user:password@example.com/")] {
            var bad = entry(); bad[field] = value; reject([bad])
        }
        let benchmark = CommandLine.arguments.contains("--benchmark")
        if benchmark {
            let input = (0..<4).map { entry("example-\($0)") }
            var times: [UInt64] = []
            for _ in 0..<1000 {
                let start = DispatchTime.now().uptimeNanoseconds
                let value = try CuratedExample.parse(input)
                precondition(value.count == 4)
                times.append(DispatchTime.now().uptimeNanoseconds - start)
            }
            times.sort()
            print("4 bundled examples, parse p99: \(times[989]) ns; excludes UIKit and remote open")
        }
        print("Curated example validation passed")
    }
}
