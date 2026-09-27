//! Writes the sample workbooks used by scripts/e2e.sh: `cargo run --example make_fixtures`
use rust_xlsxwriter::Workbook;

fn products(path: &str, rows: &[(&str, &str, f64, &str, u32, u32, &str)]) {
    let mut wb = Workbook::new();
    let s = wb.add_worksheet().set_name("Products").unwrap();
    s.write_string(0, 0, "Product catalogue").unwrap();
    s.write_string(1, 0, "Exported for the API test").unwrap();
    for (c, h) in ["SKU", "Name", "Unit Price", "In Stock", "Sales 2024", "Sales 2025", "Colour"].iter().enumerate() {
        s.write_string(3, c as u16, *h).unwrap();
    }
    for (r, (sku, name, price, stock, s24, s25, colour)) in rows.iter().enumerate() {
        let r = r as u32 + 4;
        s.write_string(r, 0, *sku).unwrap();
        s.write_string(r, 1, *name).unwrap();
        s.write_number(r, 2, *price).unwrap();
        s.write_string(r, 3, *stock).unwrap();
        s.write_number(r, 4, *s24 as f64).unwrap();
        s.write_number(r, 5, *s25 as f64).unwrap();
        s.write_string(r, 6, *colour).unwrap();
    }
    let t = wb.add_worksheet().set_name("Regions").unwrap();
    for (c, h) in ["Code", "Region"].iter().enumerate() {
        t.write_string(0, c as u16, *h).unwrap();
    }
    for (r, (code, name)) in [("N", "North"), ("S", "South")].iter().enumerate() {
        t.write_string(r as u32 + 1, 0, *code).unwrap();
        t.write_string(r as u32 + 1, 1, *name).unwrap();
    }
    wb.add_worksheet().set_name("Empty").unwrap();
    wb.save(path).unwrap();
}

fn main() {
    products("tests/fixtures/catalogue.xlsx", &[
        ("A-1", "Red widget", 2.5, "yes", 10, 12, "red"),
        ("A-2", "Blue widget", 3.75, "no", 4, 9, "blue"),
        ("B-7", "Gadget", 12.0, "yes", 30, 25, "green"),
        ("B-8", "Large gadget", 19.99, "y", 1, 3, "black"),
    ]);
    // next month's file: A-2 repriced, B-9 new, A-1/B-7/B-8 absent
    products("tests/fixtures/catalogue-october.xlsx", &[
        ("A-2", "Blue widget", 4.0, "yes", 4, 15, "blue"),
        ("B-9", "Tiny gadget", 1.25, "no", 0, 2, "white"),
        ("A-2", "Blue widget (dup)", 4.5, "yes", 0, 0, "blue"),
    ]);
    std::fs::write("tests/fixtures/prices.ssv", "item;price;updated\nPen;1,50;2026-01-02\nInk;12,25;2026-02-03\n").unwrap();
}
