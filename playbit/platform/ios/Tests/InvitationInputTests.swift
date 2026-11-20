import Foundation

@main enum InvitationInputTests {
    static func main() {
        let benchmark = CommandLine.arguments.contains("--benchmark")
        let maximum = String(repeating: "x", count: InvitationInput.maximumBytes)
        precondition(InvitationInput.canSubmit("playbit:session:public:2GkZq7nXvR4wLdT9pQ8mYc"))
        precondition(!InvitationInput.canSubmit(""))
        precondition(InvitationInput.submission(" \n\t") == nil)
        precondition(InvitationInput.submission("\u{2003}") == nil)
        precondition(InvitationInput.submission(" \nplaybit:session:public:2GkZq7nXvR4wLdT9pQ8mYc\t ") == "playbit:session:public:2GkZq7nXvR4wLdT9pQ8mYc")
        precondition(InvitationInput.canSubmit(maximum))
        precondition(!InvitationInput.canSubmit(maximum + "x"))
        precondition(InvitationInput.bounded(String(repeating: "é", count: 16384)))
        precondition(!InvitationInput.bounded(String(repeating: "é", count: 16385)))
        precondition(InvitationInput.acceptsChange(maximum, range: NSRange(location: 0, length: 1), replacement: "y"))
        precondition(!InvitationInput.acceptsChange(maximum, range: NSRange(location: 0, length: 0), replacement: "y"))
        precondition(InvitationInput.acceptsChange("Café 👩🏽‍💻", range: NSRange(location: 5, length: 7), replacement: "日本語"))
        precondition(!InvitationInput.acceptsChange("draft", range: NSRange(location: Int.max, length: 1), replacement: "x"))
        precondition(!InvitationInput.acceptsChange("draft", range: NSRange(location: 0, length: Int.max), replacement: "x"))
        precondition(!InvitationInput.acceptsChange("draft", range: NSRange(location: 0, length: 5), replacement: maximum + "x"))
        for (name, value) in [("empty", ""), ("ordinary", "https://playbit.app/s#2GkZq7nXvR4wLdT9pQ8mYc"),
                              ("maximum", maximum), ("maximum blank", String(repeating: " ", count: 32768)),
                              ("Unicode blank", String(repeating: "\u{2003}", count: 10922)),
                              ("oversized paste", String(repeating: "x", count: 1024 * 1024))] {
            var checks: [UInt64] = [], edits: [UInt64] = [], submissions: [UInt64] = []
            for _ in 0..<(benchmark ? 1000 : 1) {
                let start = benchmark ? DispatchTime.now().uptimeNanoseconds : 0
                let ready = InvitationInput.canSubmit(value)
                if benchmark { checks.append(DispatchTime.now().uptimeNanoseconds - start) }
                precondition(ready == (name != "empty" && name != "oversized paste"))
                let before = benchmark ? DispatchTime.now().uptimeNanoseconds : 0
                let accepted = InvitationInput.acceptsChange("draft", range: NSRange(location: 0, length: 5), replacement: value)
                if benchmark { edits.append(DispatchTime.now().uptimeNanoseconds - before) }
                precondition(accepted == (name != "oversized paste"))
                let submitStart = benchmark ? DispatchTime.now().uptimeNanoseconds : 0
                let submitted = InvitationInput.submission(value)
                if benchmark { submissions.append(DispatchTime.now().uptimeNanoseconds - submitStart) }
                precondition((submitted != nil) == (name == "ordinary" || name == "maximum"))
            }
            checks.sort(); edits.sort(); submissions.sort()
            if benchmark { print("invitation input \(name) n=1000 ready p99=\(checks[989]) max=\(checks[999]) ns; replace p99=\(edits[989]) max=\(edits[999]) ns; submission p99=\(submissions[989]) max=\(submissions[999]) ns") }
        }
        print("InvitationInputTests: UTF-8 limits, native UTF-16 ranges and retained-text rejection passed")
    }
}
