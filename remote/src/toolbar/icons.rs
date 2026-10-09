use super::{
    Icon, Paint, Rect, Segment, Shape, arc, circle, fill, polygon, polyline, rounded_rect, stroke,
};

fn grid_rounded_rect(x: f64, y: f64, width: f64, height: f64, radius: f64) -> Vec<Segment> {
    rounded_rect(Rect::new(x, y, width, height), radius)
}

/// The icon's shapes on a [`GRID`]-unit square grid.
pub fn icon(icon: Icon) -> Vec<Shape> {
    match icon {
        Icon::User => vec![
            stroke(circle(10.0, 6.5, 3.2)),
            stroke(vec![
                Segment::Move(3.5, 17.5),
                Segment::Cubic(3.5, 13.9, 6.4, 11.5, 10.0, 11.5),
                Segment::Cubic(13.6, 11.5, 16.5, 13.9, 16.5, 17.5),
            ]),
        ],
        Icon::Display => vec![
            stroke(grid_rounded_rect(2.5, 3.5, 15.0, 10.5, 1.8)),
            stroke(polyline(&[(10.0, 14.0), (10.0, 16.5)])),
            stroke(polyline(&[(6.5, 16.5), (13.5, 16.5)])),
        ],
        Icon::Quality => vec![
            stroke(arc(10.0, 12.5, 7.0, 150.0, 390.0)),
            stroke(polyline(&[(10.0, 12.5), (13.6, 8.4)])),
            fill(circle(10.0, 12.5, 1.4)),
        ],
        Icon::Record => vec![fill(circle(10.0, 10.0, 4.5))],
        Icon::Key => vec![
            stroke(circle(6.5, 13.5, 3.5)),
            stroke(polyline(&[(9.0, 11.0), (16.5, 3.5)])),
            stroke(polyline(&[(14.0, 6.0), (15.8, 7.8)])),
            stroke(polyline(&[(11.9, 8.1), (13.4, 9.6)])),
        ],
        Icon::Keyboard => keyboard(),
        Icon::Power => vec![
            stroke(arc(10.0, 10.8, 6.6, 300.0, 600.0)),
            stroke(polyline(&[(10.0, 2.6), (10.0, 9.6)])),
        ],
        Icon::Clipboard => clipboard(),
        Icon::Pen => vec![
            stroke(polygon(&[
                (3.0, 17.0),
                (4.0, 13.0),
                (13.5, 3.5),
                (16.5, 6.5),
                (7.0, 16.0),
            ])),
            stroke(polyline(&[(11.5, 5.5), (14.5, 8.5)])),
        ],
        Icon::Folder => folder(),
        Icon::Toolbox => vec![
            stroke(grid_rounded_rect(2.5, 7.0, 15.0, 9.5, 1.8)),
            stroke(polyline(&[
                (7.0, 7.0),
                (7.0, 4.0),
                (13.0, 4.0),
                (13.0, 7.0),
            ])),
            stroke(polyline(&[(2.5, 11.0), (17.5, 11.0)])),
            stroke(polyline(&[(10.0, 10.0), (10.0, 12.5)])),
        ],
        Icon::Chat => chat_bubble(),
        Icon::Pulse => vec![stroke(polyline(&[
            (2.0, 10.5),
            (5.5, 10.5),
            (7.5, 5.0),
            (11.0, 15.5),
            (13.0, 10.5),
            (18.0, 10.5),
        ]))],
        Icon::Gear => gear(),
        Icon::Minimize => vec![caption(polyline(&[(5.0, 10.0), (15.0, 10.0)]))],
        Icon::Maximize => vec![caption(polygon(&[
            (5.5, 5.5),
            (14.5, 5.5),
            (14.5, 14.5),
            (5.5, 14.5),
        ]))],
        Icon::Restore => restore(),
        Icon::Close => vec![
            caption(polyline(&[(5.5, 5.5), (14.5, 14.5)])),
            caption(polyline(&[(14.5, 5.5), (5.5, 14.5)])),
        ],
        Icon::Chevron => vec![Shape {
            segments: polyline(&[(4.5, 7.5), (10.0, 13.0), (15.5, 7.5)]),
            paint: Paint::Stroke(3.0),
        }],
    }
}

/// Caption glyphs are thin, like the system's.
fn caption(segments: Vec<Segment>) -> Shape {
    Shape {
        segments,
        paint: Paint::Stroke(1.0),
    }
}

fn keyboard() -> Vec<Shape> {
    let mut shapes = vec![
        stroke(grid_rounded_rect(2.0, 5.0, 16.0, 10.0, 2.0)),
        stroke(polyline(&[(6.5, 11.8), (13.5, 11.8)])),
    ];
    for x in [5.5, 8.5, 11.5, 14.5] {
        shapes.push(fill(circle(x, 8.4, 0.9)));
    }
    shapes
}

fn clipboard() -> Vec<Shape> {
    vec![
        stroke(vec![
            Segment::Move(7.5, 4.0),
            Segment::Line(6.3, 4.0),
            Segment::Cubic(5.3, 4.0, 4.5, 4.8, 4.5, 5.8),
            Segment::Line(4.5, 15.7),
            Segment::Cubic(4.5, 16.7, 5.3, 17.5, 6.3, 17.5),
            Segment::Line(13.7, 17.5),
            Segment::Cubic(14.7, 17.5, 15.5, 16.7, 15.5, 15.7),
            Segment::Line(15.5, 5.8),
            Segment::Cubic(15.5, 4.8, 14.7, 4.0, 13.7, 4.0),
            Segment::Line(12.5, 4.0),
        ]),
        stroke(grid_rounded_rect(7.5, 2.5, 5.0, 3.0, 1.0)),
        stroke(polyline(&[(7.5, 9.5), (12.5, 9.5)])),
        stroke(polyline(&[(7.5, 12.8), (10.8, 12.8)])),
    ]
}

fn folder() -> Vec<Shape> {
    vec![stroke(vec![
        Segment::Move(2.5, 6.0),
        Segment::Cubic(2.5, 5.2, 3.2, 4.5, 4.0, 4.5),
        Segment::Line(7.6, 4.5),
        Segment::Line(9.4, 6.5),
        Segment::Line(16.0, 6.5),
        Segment::Cubic(16.8, 6.5, 17.5, 7.2, 17.5, 8.0),
        Segment::Line(17.5, 14.5),
        Segment::Cubic(17.5, 15.3, 16.8, 16.0, 16.0, 16.0),
        Segment::Line(4.0, 16.0),
        Segment::Cubic(3.2, 16.0, 2.5, 15.3, 2.5, 14.5),
        Segment::Close,
    ])]
}

fn chat_bubble() -> Vec<Shape> {
    vec![stroke(vec![
        Segment::Move(4.5, 3.5),
        Segment::Line(15.5, 3.5),
        Segment::Cubic(16.6, 3.5, 17.5, 4.4, 17.5, 5.5),
        Segment::Line(17.5, 11.5),
        Segment::Cubic(17.5, 12.6, 16.6, 13.5, 15.5, 13.5),
        Segment::Line(10.5, 13.5),
        Segment::Line(6.5, 16.8),
        Segment::Line(6.5, 13.5),
        Segment::Line(4.5, 13.5),
        Segment::Cubic(3.4, 13.5, 2.5, 12.6, 2.5, 11.5),
        Segment::Line(2.5, 5.5),
        Segment::Cubic(2.5, 4.4, 3.4, 3.5, 4.5, 3.5),
        Segment::Close,
    ])]
}

fn gear() -> Vec<Shape> {
    let mut outline = Vec::with_capacity(32);
    for tooth in 0..8 {
        let center = f64::from(tooth) * 45.0;
        for (radius, offset) in [(6.3, -13.0), (8.4, -8.0), (8.4, 8.0), (6.3, 13.0)] {
            let angle = (center + offset).to_radians();
            outline.push((10.0 + radius * angle.cos(), 10.0 + radius * angle.sin()));
        }
    }
    vec![stroke(polygon(&outline)), stroke(circle(10.0, 10.0, 2.6))]
}

fn restore() -> Vec<Shape> {
    vec![
        caption(polygon(&[
            (5.5, 7.5),
            (12.5, 7.5),
            (12.5, 14.5),
            (5.5, 14.5),
        ])),
        caption(polyline(&[
            (7.5, 7.5),
            (7.5, 5.5),
            (14.5, 5.5),
            (14.5, 12.5),
            (12.5, 12.5),
        ])),
    ]
}
