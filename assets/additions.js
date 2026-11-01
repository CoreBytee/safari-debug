// Browser polyfills for running the WebKit inspector UI outside WebKit.
//
// The WebKit UI is written for Safari/WebKit and uses a few non-standard
// platform APIs. Chromium has most of them; Firefox needs these shims.

if (!Element.prototype.scrollIntoViewIfNeeded) {
    Element.prototype.scrollIntoViewIfNeeded = function(centerIfNeeded) {
        this.scrollIntoView({
            block: centerIfNeeded ? "center" : "nearest",
            inline: "nearest",
        });
    };
}

if (!Node.prototype.isInsertionCaretInside) {
    Node.prototype.isInsertionCaretInside = function() {
        const selection = window.getSelection();
        if (!selection || selection.rangeCount === 0)
            return false;
        const range = selection.getRangeAt(0);
        return this.contains(range.startContainer);
    };
}

if (!Document.prototype.getCSSCanvasContext) {
    Document.prototype.getCSSCanvasContext = function(contextType, identifier, width, height) {
        const canvas = document.createElement("canvas");
        canvas.width = width;
        canvas.height = height;
        return canvas.getContext(contextType);
    };
}

if (!CSSStyleDeclaration.prototype.getPropertyCSSValue) {
    window.CSSPrimitiveValue = window.CSSPrimitiveValue || {};
    window.CSSPrimitiveValue.CSS_PX = "px";

    CSSStyleDeclaration.prototype.getPropertyCSSValue = function(property) {
        const style = this;
        return {
            getFloatValue(unit) {
                if (unit !== window.CSSPrimitiveValue.CSS_PX)
                    throw new Error("Unit not implemented: " + unit);
                return Number(style.getPropertyValue(property).replace("px", ""));
            },
        };
    };
}
