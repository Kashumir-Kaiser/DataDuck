(text as text) as number =>
    let
        unsigned = Text.TrimStart(text, {"-"}),
        parts = Text.Split(unsigned, "."),
        fraction = if List.Count(parts) = 2 then parts{1} else "",
        digits = parts{0} & fraction,
        length = Text.Length(digits),
        // Each parsed integer chunk fits exactly in a binary64 mantissa.
        coefficient = if length <= 15 then Number.FromText(digits, "en-US") else
            Value.Add(
                Value.Multiply(Number.FromText(Text.Start(digits, length - 9), "en-US"), 1000000000, Precision.Decimal),
                Number.FromText(Text.End(digits, 9), "en-US"),
                Precision.Decimal
            ),
        signed = if Text.StartsWith(text, "-") then Value.Subtract(0, coefficient, Precision.Decimal) else coefficient
    in
        Value.Divide(signed, Number.Power(10, Text.Length(fraction)), Precision.Decimal)
